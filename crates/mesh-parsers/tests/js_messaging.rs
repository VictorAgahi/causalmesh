//! Message brokers, queues and event buses in JavaScript / TypeScript: Kafka (and
//! Redpanda), Redis, NATS, MQTT, RabbitMQ, BullMQ, SQS/SNS, Pub/Sub, Pulsar and Azure.
//! Every library is gated on the file loading it; a bare `.publish(...)` is not a broker call.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::{ContractGraph, EdgeKind, NodeKind};
use mesh_parsers::{FileIndex, PolyglotIndexer};
use std::path::Path;

fn index(file: &str, code: &str) -> FileIndex {
    let fi = PolyglotIndexer::extract(Path::new(file), code, 0);
    assert!(!fi.parse_failed, "{file} must parse");
    fi
}

/// `(role, name)` of every broker interaction, sorted: role is `P` or `C`.
fn events(fi: &FileIndex) -> Vec<(char, String)> {
    let mut out: Vec<(char, String)> = fi
        .producers
        .iter()
        .map(|(_, t)| ('P', t.to_string()))
        .chain(fi.consumers.iter().map(|(_, t)| ('C', t.to_string())))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn ev(role: char, name: &str) -> (char, String) {
    (role, name.to_string())
}

fn kinds(fi: &FileIndex, name: &str) -> Vec<NodeKind> {
    fi.nodes
        .iter()
        .filter(|n| n.name == name)
        .map(|n| n.kind)
        .collect()
}

// ---- Kafka and Redpanda ---------------------------------------------------------------

#[test]
fn kafkajs_and_redpanda_producers_and_consumers() {
    let fi = index(
        "svc/orders.ts",
        "import { Kafka } from 'kafkajs';\n\
         const kafka = new Kafka({ brokers: ['redpanda-0:9092'] });\n\
         export async function run() {\n\
           await producer.send({ topic: 'orders', messages: [] });\n\
           await producer.sendBatch({ topicMessages: [{ topic: 'audit', messages: [] }, { topic: 'billing' }] });\n\
           await consumer.subscribe({ topics: ['paid', 'refunded'], fromBeginning: true });\n\
           await consumer.subscribe({ topic: 'shipped' });\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('C', "paid"),
            ev('C', "refunded"),
            ev('C', "shipped"),
            ev('P', "audit"),
            ev('P', "billing"),
            ev('P', "orders"),
        ]
    );
    assert_eq!(kinds(&fi, "orders"), vec![NodeKind::KafkaTopic]);
}

#[test]
fn topic_constants_enums_and_templates_are_resolved_within_the_file() {
    let fi = index(
        "svc/consts.ts",
        "const ORDERS = 'orders';\n\
         const Topics = { PAID: 'paid', Nested: { A: 'nested-a' } } as const;\n\
         enum Channel { Audit = 'audit' }\n\
         class T { static readonly Shipped = 'shipped'; }\n\
         const PREFIX = 'eu';\n\
         export async function run(env: string) {\n\
           await producer.send({ topic: ORDERS });\n\
           await producer.send({ topic: Topics.PAID });\n\
           await producer.send({ topic: Topics.Nested.A });\n\
           await producer.send({ topic: Channel.Audit });\n\
           await producer.send({ topic: T.Shipped });\n\
           await producer.send({ topic: `${PREFIX}.orders` });\n\
           await producer.send({ topic: PREFIX + '.refunds' });\n\
           await producer.send({ topic: `orders.${env}` });\n\
           await producer.send({ topic: TOPIC_NAME });\n\
           await producer.send({ topic: config.topic });\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('P', "audit"),
            ev('P', "eu.orders"),
            ev('P', "eu.refunds"),
            ev('P', "nested-a"),
            ev('P', "orders"),
            ev('P', "paid"),
            ev('P', "shipped"),
        ],
        "a parameter, an undeclared name and `config.topic` are not topic names"
    );
}

#[test]
fn imported_constant_members_are_kept_for_the_graph_to_reduce() {
    let fi = index(
        "svc/shared.ts",
        "import { Topics } from '@org/events';\nimport { ORDER_PAID } from './topics';\n\
         export async function run() {\n\
           await producer.send({ topic: Topics.ORDER_PLACED });\n\
           await consumer.subscribe({ topic: ORDER_PAID });\n\
           await consumer.subscribe({ topic: Topics.lowercase });\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![ev('C', "ORDER_PAID"), ev('P', "Topics.ORDER_PLACED")]
    );
}

#[test]
fn node_rdkafka_and_confluent_clients_need_their_import() {
    let with_import = index(
        "svc/native.js",
        "const Kafka = require('node-rdkafka');\n\
         function publish() { producer.produce('orders', null, Buffer.from('x')); }\n\
         function listen() { consumer.subscribe(['paid', 'refunded']); }\n",
    );
    assert_eq!(
        events(&with_import),
        vec![ev('C', "paid"), ev('C', "refunded"), ev('P', "orders")]
    );
    let confluent = index(
        "svc/conf.ts",
        "import { KafkaJS } from '@confluentinc/kafka-javascript';\n\
         async function f() { await consumer.subscribe({ topics: ['t1'] }); await producer.send({ topic: 't2' }); }\n",
    );
    assert_eq!(events(&confluent), vec![ev('C', "t1"), ev('P', "t2")]);

    let no_import = index(
        "svc/other.js",
        "function f() { thing.produce('orders'); stream.subscribe(['a']); }\n",
    );
    assert!(events(&no_import).is_empty());
}

#[test]
fn kafka_node_and_platformatic_kafka() {
    let kafka_node = index(
        "svc/kn.js",
        "const kafka = require('kafka-node');\n\
         const { Consumer, ConsumerGroup } = kafka;\n\
         function f() { producer.send([{ topic: 'kn-out', messages: 'm' }], cb); }\n\
         const c = new kafka.Consumer(client, [{ topic: 'kn-in' }], {});\n\
         const g = new kafka.ConsumerGroup(opts, ['kn-group']);\n",
    );
    assert_eq!(
        events(&kafka_node),
        vec![ev('C', "kn-group"), ev('C', "kn-in"), ev('P', "kn-out")]
    );
    let platformatic = index(
        "svc/pf.ts",
        "import { Producer, Consumer } from '@platformatic/kafka';\n\
         async function f() { await producer.send({ messages: [{ topic: 'pf-out', value: 'x' }] }); \
         const s = await consumer.consume({ topics: ['pf-in'] }); }\n",
    );
    assert_eq!(
        events(&platformatic),
        vec![ev('C', "pf-in"), ev('P', "pf-out")]
    );
}

// ---- Redis ----------------------------------------------------------------------------

#[test]
fn redis_pub_sub_through_ioredis_and_node_redis() {
    let fi = index(
        "svc/redis.ts",
        "import Redis from 'ioredis';\nimport { createClient } from 'redis';\n\
         const pub = new Redis();\n\
         export async function notify() { await pub.publish('orders:created', JSON.stringify({})); }\n\
         export async function listen() {\n\
           sub.subscribe('orders:paid', 'orders:refunded', (err) => {});\n\
           await client.subscribe('orders:shipped', (m) => {});\n\
           await client.pSubscribe('audit:*', (m) => {});\n\
           sub.psubscribe('billing:*');\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('C', "audit:*"),
            ev('C', "billing:*"),
            ev('C', "orders:paid"),
            ev('C', "orders:refunded"),
            ev('C', "orders:shipped"),
            ev('P', "orders:created"),
        ]
    );
    assert_eq!(kinds(&fi, "orders:created"), vec![NodeKind::EventStream]);
}

#[test]
fn redis_streams_in_both_client_styles() {
    let ioredis = index(
        "svc/streams.js",
        "const Redis = require('ioredis');\n\
         async function f() {\n\
           await redis.xadd('events', '*', 'type', 'created');\n\
           await redis.xreadgroup('GROUP', 'g', 'c', 'COUNT', 10, 'BLOCK', 100, 'STREAMS', 's1', 's2', '>', '>');\n\
           await redis.xread('STREAMS', 'single', '$');\n\
         }\n",
    );
    assert_eq!(
        events(&ioredis),
        vec![
            ev('C', "s1"),
            ev('C', "s2"),
            ev('C', "single"),
            ev('P', "events")
        ]
    );
    let node_redis = index(
        "svc/streams2.ts",
        "import { createClient } from 'redis';\n\
         async function f() {\n\
           await client.xAdd('out', '*', { a: '1' });\n\
           await client.xReadGroup('g', 'c', { key: 'in1', id: '>' }, { COUNT: 1 });\n\
           await client.xRead([{ key: 'in2', id: '0' }, { key: 'in3', id: '0' }]);\n\
         }\n",
    );
    assert_eq!(
        events(&node_redis),
        vec![
            ev('C', "in1"),
            ev('C', "in2"),
            ev('C', "in3"),
            ev('P', "out")
        ]
    );
}

#[test]
fn generic_publish_subscribe_send_without_a_broker_import_record_nothing() {
    let fi = index(
        "web/handlers.ts",
        "export function handler(res, bus, obs) {\n\
           res.send('ok');\n\
           bus.publish('orders');\n\
           bus.subscribe('orders');\n\
           obs.subscribe(['a']);\n\
           emitter.emit('ready');\n\
         }\n",
    );
    assert!(events(&fi).is_empty(), "{:?}", events(&fi));
    assert!(fi.nodes.iter().all(|n| n.kind == NodeKind::ServiceClass));
}

// ---- NATS, MQTT, RabbitMQ -------------------------------------------------------------

#[test]
fn nats_core_and_jetstream_across_package_generations() {
    let classic = index(
        "svc/nats.js",
        "const { connect } = require('nats');\n\
         async function f() {\n\
           nc.publish('orders.created', data);\n\
           const r = await nc.request('orders.price', data);\n\
           const sub = nc.subscribe('orders.paid');\n\
           await js.publish('orders.jet', data);\n\
         }\n",
    );
    assert_eq!(
        events(&classic),
        vec![
            ev('C', "orders.paid"),
            ev('P', "orders.created"),
            ev('P', "orders.jet"),
            ev('P', "orders.price"),
        ]
    );
    let modern = index(
        "svc/nats2.ts",
        "import { connect } from '@nats-io/transport-node';\nimport { jetstream } from '@nats-io/jetstream';\n\
         async function f() { nc.subscribe('events.>'); await js.publish('events.x', 'y'); }\n",
    );
    assert_eq!(
        events(&modern),
        vec![ev('C', "events.>"), ev('P', "events.x")]
    );
}

#[test]
fn mqtt_topics_in_every_subscribe_shape() {
    let fi = index(
        "svc/mqtt.ts",
        "import mqtt from 'mqtt';\n\
         function f() {\n\
           client.publish('sensors/temp', '21');\n\
           client.subscribe('cmd/one');\n\
           client.subscribe(['cmd/two', 'cmd/three']);\n\
           client.subscribe({ 'cmd/four': { qos: 1 } });\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('C', "cmd/four"),
            ev('C', "cmd/one"),
            ev('C', "cmd/three"),
            ev('C', "cmd/two"),
            ev('P', "sensors/temp"),
        ]
    );
}

#[test]
fn amqplib_queues_and_exchanges() {
    let fi = index(
        "svc/amqp.js",
        "const amqp = require('amqplib');\n\
         async function f(ch) {\n\
           ch.sendToQueue('jobs', Buffer.from('x'));\n\
           ch.publish('billing', 'invoice.created', Buffer.from('x'));\n\
           ch.publish('', 'direct-queue', Buffer.from('x'));\n\
           ch.consume('results', (m) => {});\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![ev('C', "results"), ev('P', "billing"), ev('P', "jobs")]
    );
    assert_eq!(kinds(&fi, "jobs"), vec![NodeKind::Queue]);
    assert_eq!(kinds(&fi, "billing"), vec![NodeKind::EventStream]);
}

// ---- Queues ---------------------------------------------------------------------------

#[test]
fn bullmq_bull_and_bee_queue() {
    let bullmq = index(
        "svc/bullmq.ts",
        "import { Queue, Worker } from 'bullmq';\nconst NAME = 'emails';\n\
         export const q = new Queue(NAME);\n\
         export const w = new Worker('emails-out', async (job) => {});\n",
    );
    assert_eq!(
        events(&bullmq),
        vec![ev('C', "emails-out"), ev('P', "emails")]
    );

    // The historical bare form, with no import at all, still counts.
    let bare = index("svc/bare.ts", "export const q = new Queue('legacy');\n");
    assert_eq!(events(&bare), vec![ev('P', "legacy")]);

    let bull = index(
        "svc/bull.js",
        "const Bull = require('bull');\nconst q = new Bull('bull-jobs');\n",
    );
    assert_eq!(events(&bull), vec![ev('P', "bull-jobs")]);
    let bee = index(
        "svc/bee.js",
        "import BeeQueue from 'bee-queue';\nconst q = new BeeQueue('bee-jobs');\n",
    );
    assert_eq!(events(&bee), vec![ev('P', "bee-jobs")]);

    // A `Queue` from another library is not a broker queue.
    let other = index(
        "svc/other.ts",
        "import { Queue } from 'typescript-collections';\nconst q = new Queue('x');\n",
    );
    assert!(events(&other).is_empty());
}

#[test]
fn aws_sqs_and_sns_use_the_queue_or_topic_name_from_literal_urls_only() {
    let v3 = index(
        "svc/aws.ts",
        "import { SQSClient, SendMessageCommand, ReceiveMessageCommand } from '@aws-sdk/client-sqs';\n\
         import { PublishCommand } from '@aws-sdk/client-sns';\n\
         async function f() {\n\
           await sqs.send(new SendMessageCommand({ QueueUrl: 'https://sqs.eu-west-1.amazonaws.com/123456789012/orders', MessageBody: 'x' }));\n\
           await sqs.send(new ReceiveMessageCommand({ QueueUrl: 'https://sqs.eu-west-1.amazonaws.com/123456789012/results' }));\n\
           await sns.send(new PublishCommand({ TopicArn: 'arn:aws:sns:eu-west-1:123456789012:alerts', Message: 'x' }));\n\
           await sqs.send(new SendMessageCommand({ QueueUrl: process.env.QUEUE_URL }));\n\
         }\n",
    );
    assert_eq!(
        events(&v3),
        vec![ev('C', "results"), ev('P', "alerts"), ev('P', "orders")]
    );
    let v2 = index(
        "svc/aws2.js",
        "const AWS = require('aws-sdk');\n\
         function f() {\n\
           sqs.sendMessage({ QueueUrl: 'https://sqs.us-east-1.amazonaws.com/1/v2-out' }).promise();\n\
           sqs.receiveMessage({ QueueUrl: 'https://sqs.us-east-1.amazonaws.com/1/v2-in' }).promise();\n\
           sns.publish({ TopicArn: 'arn:aws:sns:us-east-1:1:v2-topic' }).promise();\n\
         }\n",
    );
    assert_eq!(
        events(&v2),
        vec![ev('C', "v2-in"), ev('P', "v2-out"), ev('P', "v2-topic")]
    );
}

#[test]
fn google_pubsub_pulsar_and_azure() {
    let pubsub = index(
        "svc/pubsub.ts",
        "import { PubSub } from '@google-cloud/pubsub';\n\
         async function f() {\n\
           await pubsub.topic('gcp-events').publishMessage({ data: Buffer.from('x') });\n\
           pubsub.topic('gcp-in').subscription('sub-1').on('message', (m) => {});\n\
         }\n",
    );
    assert_eq!(
        events(&pubsub),
        vec![ev('C', "gcp-in"), ev('P', "gcp-events")]
    );
    let pulsar = index(
        "svc/pulsar.js",
        "const Pulsar = require('pulsar-client');\n\
         async function f() {\n\
           await client.createProducer({ topic: 'persistent://public/default/p-out' });\n\
           await client.subscribe({ topic: 'p-in', subscription: 's' });\n\
         }\n",
    );
    assert_eq!(
        events(&pulsar),
        vec![
            ev('C', "p-in"),
            ev('P', "persistent://public/default/p-out")
        ]
    );
    let azure = index(
        "svc/azure.ts",
        "import { ServiceBusClient } from '@azure/service-bus';\n\
         import { EventHubProducerClient, EventHubConsumerClient } from '@azure/event-hubs';\n\
         async function f() {\n\
           const s = client.createSender('sb-out');\n\
           const r = client.createReceiver('sb-in');\n\
           new EventHubProducerClient('Endpoint=sb://x/;SharedAccessKey=k', 'hub-out');\n\
           new EventHubConsumerClient('$Default', 'Endpoint=sb://x/;SharedAccessKey=k', 'hub-in');\n\
         }\n",
    );
    assert_eq!(
        events(&azure),
        vec![
            ev('C', "hub-in"),
            ev('C', "sb-in"),
            ev('P', "hub-out"),
            ev('P', "sb-out")
        ]
    );
}

#[test]
fn nestjs_clients_need_a_client_shaped_receiver() {
    let fi = index(
        "svc/nest.ts",
        "export class S {\n\
           notify() { this.client.emit('user.created', {}); }\n\
           ask() { return this.kafkaProxy.send('user.get', {}); }\n\
           reply(res) { res.send('ok'); }\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![ev('P', "user.created"), ev('P', "user.get")]
    );
    let typed = index(
        "svc/nest2.ts",
        "import { ClientProxy } from '@nestjs/microservices';\n\
         export class S { go() { this.bus.emit('any.thing', {}); } }\n",
    );
    assert_eq!(events(&typed), vec![ev('P', "any.thing")]);
}

// ---- wiring ---------------------------------------------------------------------------

#[test]
fn an_interaction_is_attributed_to_the_declaration_that_makes_it() {
    let fi = index(
        "svc/owner.ts",
        "import Redis from 'ioredis';\n\
         export class Notifier {\n\
           async notify() { await this.redis.publish('chan', 'x'); }\n\
           async listen() { this.sub.subscribe('other'); }\n\
         }\n",
    );
    let name_of = |i: usize| fi.nodes[i].name.to_string();
    let produced: Vec<(String, String)> = fi
        .producers
        .iter()
        .map(|(i, t)| (name_of(*i), t.to_string()))
        .collect();
    let consumed: Vec<(String, String)> = fi
        .consumers
        .iter()
        .map(|(i, t)| (name_of(*i), t.to_string()))
        .collect();
    assert_eq!(produced, vec![("notify".to_string(), "chan".to_string())]);
    assert_eq!(consumed, vec![("listen".to_string(), "other".to_string())]);
}

#[test]
fn a_redis_producer_and_a_nats_free_consumer_meet_on_one_topic_in_the_graph() {
    let mut graph = ContractGraph::new();
    PolyglotIndexer::extract(
        Path::new("producer/pub.ts"),
        "import Redis from 'ioredis';\nexport async function notify(r: Redis) { await r.publish('orders:created', 'x'); }\n",
        0,
    )
    .apply(&mut graph);
    PolyglotIndexer::extract(
        Path::new("consumer/sub.ts"),
        "import Redis from 'ioredis';\nexport function listen(r: Redis) { r.subscribe('orders:created'); }\n",
        1,
    )
    .apply(&mut graph);
    graph.reconcile_edges();
    let name = |id| {
        graph
            .get_node(id)
            .map(|n| n.name.to_string())
            .unwrap_or_default()
    };
    let produces = graph.all_edges().iter().any(|e| {
        e.kind == EdgeKind::Produces && name(e.from) == "notify" && name(e.to) == "orders:created"
    });
    let consumes = graph.all_edges().iter().any(|e| {
        e.kind == EdgeKind::Consumes && name(e.from) == "orders:created" && name(e.to) == "listen"
    });
    assert!(produces, "notify -> orders:created");
    assert!(consumes, "orders:created -> listen");
}
