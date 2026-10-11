//! Message brokers, queues and event buses as JavaScript libraries expose them.
//!
//! Redpanda speaks the Kafka protocol, so it is covered through the Kafka client
//! libraries; Redis through `ioredis` / `redis` / `@redis/client` (pub/sub and
//! streams); NATS through `nats` and `@nats-io/*`; and so on. Each row says which
//! module the file must load, which method or constructor it is, which side of a
//! topic it is on, and how to read the name off the arguments. A name that is not
//! knowable from the file (a parameter, `process.env.X`) yields no row at all.

use super::scope::unquote;
use super::{CallRule, CtorRule, DecoratorRule, Role, Site};
use mesh_core::NodeKind;
use tree_sitter::Node;

// ---- argument readers -------------------------------------------------------------

/// Names in the `index`-th argument: a scalar or an array.
fn arg_names(s: &Site, index: usize) -> Vec<String> {
    s.args
        .get(index)
        .map(|a| s.names.resolve_all(*a))
        .unwrap_or_default()
}

fn first_arg(s: &Site) -> Vec<String> {
    arg_names(s, 0)
}

/// A queue name: not a script path (`new Worker('./worker.js')` is a web worker).
fn queue_name_arg(s: &Site) -> Vec<String> {
    first_arg(s)
        .into_iter()
        .filter(|n| !n.contains('/') && !n.ends_with(".js") && !n.ends_with(".ts"))
        .collect()
}

/// Every string argument (`redis.subscribe('a', 'b', callback)`): callbacks and
/// option objects resolve to nothing and are skipped.
fn all_string_args(s: &Site) -> Vec<String> {
    s.args
        .iter()
        .flat_map(|a| s.names.resolve_all(*a))
        .collect()
}

/// Every string argument but a numeric one: `brpop('q', '5')` has a timeout, not a key.
fn key_args(s: &Site) -> Vec<String> {
    all_string_args(s)
        .into_iter()
        .filter(|n| !n.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .collect()
}

/// Names under `keys` of the object at argument `index`.
fn object_keys(s: &Site, index: usize, keys: &[&str]) -> Vec<String> {
    let Some(object) = s.args.get(index) else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|k| s.names.object_value(*object, k))
        .flat_map(|v| s.names.resolve_all(v))
        .collect()
}

/// `topic` of every object in `array`, an array literal.
fn topic_of_each(s: &Site, array: Node) -> Vec<String> {
    let array = super::super::typescript::TypeScriptExtractor::peel_wrappers(array);
    if array.kind() != "array" {
        return Vec::new();
    }
    array
        .named_children(&mut array.walk())
        .filter_map(|o| s.names.object_value(o, "topic"))
        .flat_map(|v| s.names.resolve_all(v))
        .collect()
}

/// kafkajs `producer.send({ topic })`.
fn kafka_send_topic(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["topic"])
}

/// kafkajs `consumer.subscribe({ topic | topics })`.
fn kafka_subscribe_topics(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["topic", "topics"])
}

/// kafkajs `producer.sendBatch({ topicMessages: [{ topic }] })`.
fn kafka_send_batch(s: &Site) -> Vec<String> {
    s.args
        .first()
        .and_then(|o| s.names.object_value(*o, "topicMessages"))
        .map(|v| topic_of_each(s, v))
        .unwrap_or_default()
}

/// kafka-node `producer.send([{ topic, messages }])`.
fn kafka_node_send(s: &Site) -> Vec<String> {
    s.args
        .first()
        .map(|a| topic_of_each(s, *a))
        .unwrap_or_default()
}

/// @platformatic/kafka `producer.send({ messages: [{ topic }] })`.
fn platformatic_send(s: &Site) -> Vec<String> {
    s.args
        .first()
        .and_then(|o| s.names.object_value(*o, "messages"))
        .map(|v| topic_of_each(s, v))
        .unwrap_or_default()
}

/// kafka-node `new Consumer(client, [{ topic }], options)`.
fn kafka_node_consumer(s: &Site) -> Vec<String> {
    s.args
        .get(1)
        .map(|a| topic_of_each(s, *a))
        .unwrap_or_default()
}

/// kafka-node `new ConsumerGroup(options, 'topic' | ['a', 'b'])`.
fn second_arg(s: &Site) -> Vec<String> {
    arg_names(s, 1)
}

/// node-redis / ioredis stream reads. ioredis lists keys then ids after a literal
/// `'STREAMS'`; node-redis passes `{ key, id }` objects (or an array of them).
fn redis_stream_read(s: &Site) -> Vec<String> {
    let streams_at = s.args.iter().position(|a| {
        s.names
            .resolve(*a)
            .is_some_and(|v| v.eq_ignore_ascii_case("STREAMS"))
    });
    if let Some(i) = streams_at {
        let rest = &s.args[i + 1..];
        return rest[..rest.len() / 2]
            .iter()
            .filter_map(|a| s.names.resolve(*a))
            .collect();
    }
    let mut out = Vec::new();
    for arg in &s.args {
        let arg = super::super::typescript::TypeScriptExtractor::peel_wrappers(*arg);
        let objects: Vec<Node> = if arg.kind() == "array" {
            arg.named_children(&mut arg.walk()).collect()
        } else {
            vec![arg]
        };
        for o in objects {
            if s.names.object_value(o, "id").is_some() {
                if let Some(key) = s.names.object_value(o, "key") {
                    out.extend(s.names.resolve_all(key));
                }
            }
        }
    }
    out
}

/// MQTT `client.subscribe('t' | ['a', 'b'] | { 'a': { qos: 1 } })`.
fn mqtt_subscribe(s: &Site) -> Vec<String> {
    let mut out = first_arg(s);
    if let Some(object) = s.args.first() {
        let object = super::super::typescript::TypeScriptExtractor::peel_wrappers(*object);
        if object.kind() == "object" {
            for pair in object
                .named_children(&mut object.walk())
                .filter(|p| p.kind() == "pair")
            {
                if let Some(key) = pair.child_by_field_name("key") {
                    let name = if key.kind() == "string" {
                        unquote(key, s.source)
                    } else {
                        s.names.resolve(key)
                    };
                    out.extend(name);
                }
            }
        }
    }
    out
}

/// amqplib `channel.publish(exchange, routingKey, content)`: the exchange, unless
/// it is the default one (`''`), which routes by queue name and is not a topic.
fn amqp_exchange(s: &Site) -> Vec<String> {
    first_arg(s).into_iter().filter(|n| !n.is_empty()).collect()
}

/// `https://sqs.eu-west-1.amazonaws.com/123456789012/orders` -> `orders`.
fn sqs_queue(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["QueueUrl"])
        .into_iter()
        .filter_map(|u| u.rsplit('/').next().map(str::to_string))
        .filter(|n| !n.is_empty())
        .collect()
}

/// `arn:aws:sns:eu-west-1:123456789012:orders` -> `orders`.
fn sns_topic(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["TopicArn"])
        .into_iter()
        .filter(|a| a.starts_with("arn:"))
        .filter_map(|a| a.rsplit(':').next().map(str::to_string))
        .filter(|n| !n.is_empty())
        .collect()
}

/// The topic of `pubsub.topic('t').<method>()`.
fn pubsub_topic_chain(s: &Site) -> Vec<String> {
    let Some(receiver) = s.callee.child_by_field_name("object") else {
        return Vec::new();
    };
    if receiver.kind() != "call_expression" {
        return Vec::new();
    }
    let is_topic_call = receiver
        .child_by_field_name("function")
        .filter(|f| f.kind() == "member_expression")
        .and_then(|f| f.child_by_field_name("property"))
        .and_then(|p| p.utf8_text(s.source).ok())
        == Some("topic");
    if !is_topic_call {
        return Vec::new();
    }
    receiver
        .child_by_field_name("arguments")
        .and_then(|a| a.named_child(0))
        .map(|n| s.names.resolve_all(n))
        .unwrap_or_default()
}

/// Azure Event Hubs: the hub name is the last string argument that is not a
/// connection string (`new EventHubProducerClient(connectionString, 'hub')`).
fn event_hub_name(s: &Site) -> Vec<String> {
    s.args
        .iter()
        .rev()
        .filter_map(|a| s.names.resolve(*a))
        .find(|v| !v.contains('=') && !v.contains(';'))
        .into_iter()
        .collect()
}

/// A NestJS microservice client: a property or parameter typed `ClientProxy` /
/// `ClientKafka` / …, injected with `@Inject(…)`, or, in a file that loads
/// `@nestjs/microservices`, named like a client (`this.client`, `orderProxy`). An
/// event bus, a socket, a response object or a `client` in a file that does not load
/// the microservices package is none of these.
fn nest_client(s: &Site) -> bool {
    let Some(key) = s.receiver_key() else {
        return false;
    };
    if s.fx.receivers.is_client(NEST[0], &key) == Some(true) {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    s.fx.bindings.imports_any(NEST) && (lower.contains("client") || lower.contains("proxy"))
}

/// Whether the receiver chain of the call contains a call (`http.post(url).send(…)`).
fn receiver_has_call(s: &Site) -> bool {
    let mut node = s.callee.child_by_field_name("object");
    while let Some(n) = node {
        match n.kind() {
            "call_expression" => return true,
            "member_expression" => node = n.child_by_field_name("object"),
            _ => return false,
        }
    }
    false
}

/// The kafkajs-shaped `send` / `subscribe` / `sendBatch`: a kafka client when the
/// file loads a Kafka library and the receiver is one it built, or, with no Kafka
/// import, when the receiver is *named* like one (`producer`, `consumer`, `kafka`,
/// `redpanda`) and is not the tail of a call chain (`request(app).post(…).send({…})`).
fn kafka_receiver(s: &Site) -> bool {
    let key = s.receiver_key();
    if s.fx.bindings.imports_any(KAFKA_ANY) {
        return s
            .fx
            .receivers
            .is_client(KAFKAJS[0], key.as_deref().unwrap_or(""))
            .unwrap_or(true);
    }
    if receiver_has_call(s) {
        return false;
    }
    key.is_some_and(|k| {
        let k = k.to_ascii_lowercase();
        ["producer", "consumer", "kafka", "redpanda"]
            .iter()
            .any(|n| k.contains(n))
    })
}

/// Bull / bee-queue `queue.process(fn)`: the queue the receiver was built for.
fn queue_of_receiver(s: &Site) -> Vec<String> {
    s.receiver_key()
        .and_then(|k| s.fx.receivers.queue_name(&k).map(str::to_string))
        .into_iter()
        .collect()
}

/// `@Processor('q')` / `@Processor({ name: 'q' })`.
fn processor_queue(s: &Site) -> Vec<String> {
    let mut names = first_arg(s);
    names.extend(object_keys(s, 0, &["name"]));
    names
}

/// `@RabbitSubscribe({ queue, exchange, routingKey })`: the queue, else the routing key.
fn rabbit_queue(s: &Site) -> Vec<String> {
    let queue = object_keys(s, 0, &["queue"]);
    if queue.is_empty() {
        object_keys(s, 0, &["routingKey"])
    } else {
        queue
    }
}

/// AWS Kinesis: the stream of a record / shard-iterator command.
fn kinesis_stream(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["StreamName"])
}

/// AWS EventBridge `PutEventsCommand({ Entries: [{ DetailType, EventBusName, … }] })`: the event
/// type consumers match on, else the bus.
fn eventbridge_events(s: &Site) -> Vec<String> {
    let Some(entries) = s
        .args
        .first()
        .and_then(|o| s.names.object_value(*o, "Entries"))
    else {
        return Vec::new();
    };
    let entries = super::super::typescript::TypeScriptExtractor::peel_wrappers(entries);
    if entries.kind() != "array" {
        return Vec::new();
    }
    entries
        .named_children(&mut entries.walk())
        .flat_map(|entry| {
            ["DetailType", "EventBusName"]
                .iter()
                .find_map(|k| s.names.object_value(entry, k))
                .map(|v| s.names.resolve_all(v))
                .unwrap_or_default()
        })
        .collect()
}

/// STOMP `client.publish({ destination: '/queue/x', body })`.
fn stomp_destination(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["destination"])
}

// ---- libraries --------------------------------------------------------------------

const KAFKAJS: &[&str] = &["kafkajs", "@confluentinc/kafka-javascript"];
const NEST: &[&str] = &["@nestjs/microservices"];
const KAFKA_ANY: &[&str] = &[
    "kafkajs",
    "@confluentinc/kafka-javascript",
    "node-rdkafka",
    "kafka-node",
    "@platformatic/kafka",
    "@blizzard/rdkafka",
];
const BULL_QUEUES: &[&str] = &["bull", "bee-queue"];
const KAFKA_NATIVE: &[&str] = &[
    "node-rdkafka",
    "@confluentinc/kafka-javascript",
    "@confluentinc/kafka-javascript/",
    "@blizzard/rdkafka",
];
const REDIS: &[&str] = &[
    "ioredis",
    "redis",
    "@redis/client",
    "@upstash/redis",
    "ioredis-mock",
    "redis-mock",
    "@nestjs-modules/ioredis",
    "@liaoliaots/nestjs-redis",
    "@songkeys/nestjs-redis",
];
const NATS: &[&str] = &["nats", "nats.ws", "@nats-io/"];
const MQTT: &[&str] = &["mqtt", "async-mqtt"];
const AMQP: &[&str] = &["amqplib", "amqp-connection-manager"];
const SQS: &[&str] = &["@aws-sdk/client-sqs"];
const SNS: &[&str] = &["@aws-sdk/client-sns"];
const AWS_V2: &[&str] = &["aws-sdk"];
const PUBSUB: &[&str] = &["@google-cloud/pubsub"];
const PULSAR: &[&str] = &["pulsar-client"];
const SERVICE_BUS: &[&str] = &["@azure/service-bus"];
const KINESIS: &[&str] = &["@aws-sdk/client-kinesis"];
const EVENTBRIDGE: &[&str] = &["@aws-sdk/client-eventbridge"];
const STOMP: &[&str] = &[
    "@stomp/stompjs",
    "@stomp/rx-stomp",
    "stompjs",
    "stomp-client",
];
const EVENT_HUBS: &[&str] = &["@azure/event-hubs"];
const KAFKA_NODE: &[&str] = &["kafka-node"];
const PLATFORMATIC: &[&str] = &["@platformatic/kafka"];
const BULLMQ: &[&str] = &["bullmq", "@nestjs/bullmq"];

/// Library groups whose clients are tracked (the first name identifies the group).
pub(super) const FAMILIES: &[&[&str]] = &[
    KAFKAJS,
    KAFKA_NATIVE,
    KAFKA_NODE,
    PLATFORMATIC,
    REDIS,
    NATS,
    MQTT,
    AMQP,
    AWS_V2,
    PUBSUB,
    PULSAR,
    SERVICE_BUS,
    STOMP,
    NEST,
];

macro_rules! call {
    ($libs:expr, $method:literal, $role:ident, $kind:ident, $sig:literal, $extract:expr) => {
        CallRule {
            libs: $libs,
            method: $method,
            role: Role::$role,
            kind: NodeKind::$kind,
            signature: $sig,
            extract: $extract,
            when: None,
        }
    };
}

macro_rules! ctor {
    ($libs:expr, $name:expr, $unbound:expr, $role:ident, $kind:ident, $sig:literal, $extract:expr) => {
        CtorRule {
            libs: $libs,
            name: $name,
            allow_unbound: $unbound,
            role: Role::$role,
            kind: NodeKind::$kind,
            signature: $sig,
            extract: $extract,
        }
    };
}

pub(super) const CALL_RULES: &[CallRule] = &[
    // Kafka (and Redpanda, which speaks its protocol). kafkajs-shaped calls are
    // recognised by the `topic` key alone: the historical, ungated detection.
    CallRule {
        libs: &[],
        method: "send",
        role: Role::Produce,
        kind: NodeKind::KafkaTopic,
        signature: "kafkajs producer.send",
        extract: kafka_send_topic,
        when: Some(kafka_receiver),
    },
    CallRule {
        libs: &[],
        method: "subscribe",
        role: Role::Consume,
        kind: NodeKind::KafkaTopic,
        signature: "kafkajs consumer.subscribe",
        extract: kafka_subscribe_topics,
        when: Some(kafka_receiver),
    },
    CallRule {
        libs: &[],
        method: "sendBatch",
        role: Role::Produce,
        kind: NodeKind::KafkaTopic,
        signature: "kafkajs producer.sendBatch",
        extract: kafka_send_batch,
        when: Some(kafka_receiver),
    },
    call!(
        KAFKA_NATIVE,
        "produce",
        Produce,
        KafkaTopic,
        "node-rdkafka producer.produce",
        first_arg
    ),
    call!(
        KAFKA_NATIVE,
        "subscribe",
        Consume,
        KafkaTopic,
        "node-rdkafka consumer.subscribe",
        first_arg
    ),
    call!(
        KAFKA_NODE,
        "send",
        Produce,
        KafkaTopic,
        "kafka-node producer.send",
        kafka_node_send
    ),
    call!(
        PLATFORMATIC,
        "send",
        Produce,
        KafkaTopic,
        "platformatic producer.send",
        platformatic_send
    ),
    call!(
        PLATFORMATIC,
        "consume",
        Consume,
        KafkaTopic,
        "platformatic consumer.consume",
        kafka_subscribe_topics
    ),
    // Bull / bee-queue consumers: `queue.process(fn)` on a queue built in this file.
    call!(
        BULL_QUEUES,
        "process",
        Consume,
        Queue,
        "bull queue.process",
        queue_of_receiver
    ),
    // NestJS microservice clients (any transport).
    CallRule {
        libs: &[],
        method: "emit",
        role: Role::Produce,
        kind: NodeKind::KafkaTopic,
        signature: "nestjs client.emit",
        extract: first_arg,
        when: Some(nest_client),
    },
    CallRule {
        libs: &[],
        method: "send",
        role: Role::Produce,
        kind: NodeKind::KafkaTopic,
        signature: "nestjs client.send",
        extract: first_arg,
        when: Some(nest_client),
    },
    // Redis pub/sub and streams.
    call!(
        REDIS,
        "publish",
        Produce,
        EventStream,
        "redis publish",
        first_arg
    ),
    call!(
        REDIS,
        "subscribe",
        Consume,
        EventStream,
        "redis subscribe",
        all_string_args
    ),
    call!(
        REDIS,
        "psubscribe",
        Consume,
        EventStream,
        "redis psubscribe",
        all_string_args
    ),
    call!(
        REDIS,
        "pSubscribe",
        Consume,
        EventStream,
        "redis pSubscribe",
        all_string_args
    ),
    call!(REDIS, "xadd", Produce, EventStream, "redis xadd", first_arg),
    call!(REDIS, "xAdd", Produce, EventStream, "redis xAdd", first_arg),
    call!(
        REDIS,
        "xread",
        Consume,
        EventStream,
        "redis xread",
        redis_stream_read
    ),
    call!(
        REDIS,
        "xRead",
        Consume,
        EventStream,
        "redis xRead",
        redis_stream_read
    ),
    call!(
        REDIS,
        "xreadgroup",
        Consume,
        EventStream,
        "redis xreadgroup",
        redis_stream_read
    ),
    call!(
        REDIS,
        "xReadGroup",
        Consume,
        EventStream,
        "redis xReadGroup",
        redis_stream_read
    ),
    // NATS (core and JetStream share the verbs).
    call!(
        NATS,
        "publish",
        Produce,
        EventStream,
        "nats publish",
        first_arg
    ),
    call!(
        NATS,
        "request",
        Produce,
        EventStream,
        "nats request",
        first_arg
    ),
    call!(
        NATS,
        "subscribe",
        Consume,
        EventStream,
        "nats subscribe",
        first_arg
    ),
    // MQTT.
    call!(
        MQTT,
        "publish",
        Produce,
        EventStream,
        "mqtt publish",
        first_arg
    ),
    call!(
        MQTT,
        "subscribe",
        Consume,
        EventStream,
        "mqtt subscribe",
        mqtt_subscribe
    ),
    call!(
        MQTT,
        "subscribeAsync",
        Consume,
        EventStream,
        "mqtt subscribeAsync",
        mqtt_subscribe
    ),
    // RabbitMQ.
    call!(
        AMQP,
        "sendToQueue",
        Produce,
        Queue,
        "amqp sendToQueue",
        first_arg
    ),
    call!(AMQP, "consume", Consume, Queue, "amqp consume", first_arg),
    call!(
        AMQP,
        "publish",
        Produce,
        EventStream,
        "amqp publish",
        amqp_exchange
    ),
    // AWS SQS / SNS (v2 clients).
    call!(
        AWS_V2,
        "sendMessage",
        Produce,
        Queue,
        "aws-sdk sqs.sendMessage",
        sqs_queue
    ),
    call!(
        AWS_V2,
        "sendMessageBatch",
        Produce,
        Queue,
        "aws-sdk sqs.sendMessageBatch",
        sqs_queue
    ),
    call!(
        AWS_V2,
        "receiveMessage",
        Consume,
        Queue,
        "aws-sdk sqs.receiveMessage",
        sqs_queue
    ),
    call!(
        AWS_V2,
        "publish",
        Produce,
        EventStream,
        "aws-sdk sns.publish",
        sns_topic
    ),
    // Google Cloud Pub/Sub.
    call!(
        PUBSUB,
        "publishMessage",
        Produce,
        EventStream,
        "pubsub topic.publishMessage",
        pubsub_topic_chain
    ),
    call!(
        PUBSUB,
        "publishJSON",
        Produce,
        EventStream,
        "pubsub topic.publishJSON",
        pubsub_topic_chain
    ),
    call!(
        PUBSUB,
        "publish",
        Produce,
        EventStream,
        "pubsub topic.publish",
        pubsub_topic_chain
    ),
    call!(
        PUBSUB,
        "subscription",
        Consume,
        EventStream,
        "pubsub topic.subscription",
        pubsub_topic_chain
    ),
    // Apache Pulsar.
    call!(
        PULSAR,
        "createProducer",
        Produce,
        EventStream,
        "pulsar createProducer",
        pulsar_topic
    ),
    call!(
        PULSAR,
        "subscribe",
        Consume,
        EventStream,
        "pulsar subscribe",
        pulsar_topic
    ),
    call!(
        PULSAR,
        "createReader",
        Consume,
        EventStream,
        "pulsar createReader",
        pulsar_topic
    ),
    // Azure Service Bus.
    call!(
        SERVICE_BUS,
        "createSender",
        Produce,
        Queue,
        "servicebus createSender",
        first_arg
    ),
    call!(
        SERVICE_BUS,
        "createReceiver",
        Consume,
        Queue,
        "servicebus createReceiver",
        first_arg
    ),
    // Redis lists used as queues: `lpush` / `rpush` produce, `brpop` / `blpop` / `rpop` / `lpop` consume.
    call!(REDIS, "lpush", Produce, Queue, "redis lpush", first_arg),
    call!(REDIS, "lPush", Produce, Queue, "redis lPush", first_arg),
    call!(REDIS, "rpush", Produce, Queue, "redis rpush", first_arg),
    call!(REDIS, "rPush", Produce, Queue, "redis rPush", first_arg),
    call!(REDIS, "brpop", Consume, Queue, "redis brpop", key_args),
    call!(REDIS, "bRPop", Consume, Queue, "redis bRPop", key_args),
    call!(REDIS, "blpop", Consume, Queue, "redis blpop", key_args),
    call!(REDIS, "bLPop", Consume, Queue, "redis bLPop", key_args),
    call!(REDIS, "rpop", Consume, Queue, "redis rpop", first_arg),
    call!(REDIS, "rPop", Consume, Queue, "redis rPop", first_arg),
    call!(REDIS, "lpop", Consume, Queue, "redis lpop", first_arg),
    call!(REDIS, "lPop", Consume, Queue, "redis lPop", first_arg),
    // STOMP (ActiveMQ, Artemis, RabbitMQ STOMP): destinations are queues or topics.
    call!(
        STOMP,
        "publish",
        Produce,
        EventStream,
        "stomp publish",
        stomp_destination
    ),
    call!(
        STOMP,
        "subscribe",
        Consume,
        EventStream,
        "stomp subscribe",
        first_arg
    ),
];

/// Pulsar `{ topic | topics }` of the options object.
fn pulsar_topic(s: &Site) -> Vec<String> {
    object_keys(s, 0, &["topic", "topics"])
}

pub(super) const CTOR_RULES: &[CtorRule] = &[
    // BullMQ: `new Queue('q')` / `new Worker('q', …)`; also the bare form with no import.
    ctor!(
        BULLMQ,
        Some("Queue"),
        true,
        Produce,
        Queue,
        "bullmq new Queue()",
        queue_name_arg
    ),
    ctor!(
        BULLMQ,
        Some("Worker"),
        true,
        Consume,
        Queue,
        "bullmq new Worker()",
        queue_name_arg
    ),
    // Bull and bee-queue export the queue class as their default.
    ctor!(
        &["bull"],
        None,
        false,
        Produce,
        Queue,
        "bull new Queue()",
        first_arg
    ),
    ctor!(
        &["bee-queue"],
        None,
        false,
        Produce,
        Queue,
        "bee-queue new Queue()",
        first_arg
    ),
    // kafka-node consumers take their topics in the constructor.
    ctor!(
        KAFKA_NODE,
        Some("Consumer"),
        false,
        Consume,
        KafkaTopic,
        "kafka-node new Consumer()",
        kafka_node_consumer
    ),
    ctor!(
        KAFKA_NODE,
        Some("ConsumerGroup"),
        false,
        Consume,
        KafkaTopic,
        "kafka-node new ConsumerGroup()",
        second_arg
    ),
    // Azure Event Hubs.
    ctor!(
        EVENT_HUBS,
        Some("EventHubProducerClient"),
        false,
        Produce,
        EventStream,
        "eventhubs new EventHubProducerClient()",
        event_hub_name
    ),
    ctor!(
        EVENT_HUBS,
        Some("EventHubConsumerClient"),
        false,
        Consume,
        EventStream,
        "eventhubs new EventHubConsumerClient()",
        event_hub_name
    ),
    // AWS SDK v3 commands.
    ctor!(
        SQS,
        Some("SendMessageCommand"),
        false,
        Produce,
        Queue,
        "sqs new SendMessageCommand()",
        sqs_queue
    ),
    ctor!(
        SQS,
        Some("SendMessageBatchCommand"),
        false,
        Produce,
        Queue,
        "sqs new SendMessageBatchCommand()",
        sqs_queue
    ),
    ctor!(
        SQS,
        Some("ReceiveMessageCommand"),
        false,
        Consume,
        Queue,
        "sqs new ReceiveMessageCommand()",
        sqs_queue
    ),
    ctor!(
        SNS,
        Some("PublishCommand"),
        false,
        Produce,
        EventStream,
        "sns new PublishCommand()",
        sns_topic
    ),
    ctor!(
        SNS,
        Some("PublishBatchCommand"),
        false,
        Produce,
        EventStream,
        "sns new PublishBatchCommand()",
        sns_topic
    ),
    // AWS Kinesis commands and EventBridge events.
    ctor!(
        KINESIS,
        Some("PutRecordCommand"),
        false,
        Produce,
        EventStream,
        "kinesis new PutRecordCommand()",
        kinesis_stream
    ),
    ctor!(
        KINESIS,
        Some("PutRecordsCommand"),
        false,
        Produce,
        EventStream,
        "kinesis new PutRecordsCommand()",
        kinesis_stream
    ),
    ctor!(
        KINESIS,
        Some("GetShardIteratorCommand"),
        false,
        Consume,
        EventStream,
        "kinesis new GetShardIteratorCommand()",
        kinesis_stream
    ),
    ctor!(
        EVENTBRIDGE,
        Some("PutEventsCommand"),
        false,
        Produce,
        EventStream,
        "eventbridge new PutEventsCommand()",
        eventbridge_events
    ),
];

pub(super) const DECORATOR_RULES: &[DecoratorRule] = &[
    // @nestjs/bull and @nestjs/bullmq.
    DecoratorRule {
        libs: &["@nestjs/bull", "@nestjs/bullmq"],
        name: "Processor",
        role: Role::Consume,
        kind: NodeKind::Queue,
        signature: "nestjs-bull @Processor",
        extract: processor_queue,
    },
    DecoratorRule {
        libs: &["@nestjs/bull", "@nestjs/bullmq"],
        name: "InjectQueue",
        role: Role::Produce,
        kind: NodeKind::Queue,
        signature: "nestjs-bull @InjectQueue",
        extract: first_arg,
    },
    // @golevelup/nestjs-rabbitmq.
    DecoratorRule {
        libs: &["@golevelup/nestjs-rabbitmq"],
        name: "RabbitSubscribe",
        role: Role::Consume,
        kind: NodeKind::Queue,
        signature: "rabbitmq @RabbitSubscribe",
        extract: rabbit_queue,
    },
    DecoratorRule {
        libs: &["@golevelup/nestjs-rabbitmq"],
        name: "RabbitRPC",
        role: Role::Consume,
        kind: NodeKind::Queue,
        signature: "rabbitmq @RabbitRPC",
        extract: rabbit_queue,
    },
    // @ssut/nestjs-sqs.
    DecoratorRule {
        libs: &["@ssut/nestjs-sqs"],
        name: "SqsMessageHandler",
        role: Role::Consume,
        kind: NodeKind::Queue,
        signature: "sqs @SqsMessageHandler",
        extract: first_arg,
    },
];

/// Whether `source` could hold a broker interaction any rule recognises: it names one
/// of their libraries or decorators, or the historical import-free shapes (a `Queue` /
/// `Worker`, a `producer` / `consumer` / `kafka` receiver).
pub(super) fn mentions_broker(source: &str) -> bool {
    const SHAPES: &[&str] = &[
        "Queue", "Worker", "roducer", "onsumer", "afka", "edpanda", "PRODUCER", "CONSUMER",
        "KAFKA", "REDPANDA",
    ];
    if SHAPES.iter().any(|n| source.contains(n)) {
        return true;
    }
    let mentions_lib = |libs: &[&str]| {
        libs.iter()
            .any(|lib| source.contains(lib.trim_end_matches('/')))
    };
    FAMILIES.iter().any(|libs| mentions_lib(libs))
        || mentions_lib(BULLMQ)
        || mentions_lib(BULL_QUEUES)
        || CTOR_RULES.iter().any(|r| mentions_lib(r.libs))
        || DECORATOR_RULES.iter().any(|r| mentions_lib(r.libs))
}
