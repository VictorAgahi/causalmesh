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
         const producer = kafka.producer();\n\
         const consumer = kafka.consumer({ groupId: 'g' });\n\
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
         const producer = new kafka.Producer(client);\n\
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
         const sub = new Redis();\n\
         const client = createClient();\n\
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
        "import { ClientProxy } from '@nestjs/microservices';\n\
         export class S {\n\
           notify() { this.client.emit('user.created', {}); }\n\
           ask() { return this.kafkaProxy.send('user.get', {}); }\n\
           reply(res) { res.send('ok'); }\n\
         }\n",
    );
    // The file loads the microservices package: client-shaped receivers count, `res` does not.
    assert_eq!(
        events(&fi),
        vec![ev('P', "user.created"), ev('P', "user.get")]
    );
    let typed = index(
        "svc/nest2.ts",
        "import { ClientProxy } from '@nestjs/microservices';\n\
         export class S { go() { this.bus.emit('any.thing', {}); this.userClient.emit('user.x', {}); } }\n",
    );
    // The import alone does not make an arbitrary receiver a microservice client.
    assert_eq!(events(&typed), vec![ev('P', "user.x")]);
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

// ---- review round 1 (PR #82) ------------------------------------------------------------

#[test]
fn a_nest_hybrid_controller_does_not_turn_responses_and_emitters_into_topics() {
    let fi = index(
        "svc/hybrid.controller.ts",
        "import { MessagePattern } from '@nestjs/microservices';\n\
         export class C {\n\
           @MessagePattern('sum') sum() { return 1; }\n\
           reply(res, socket) {\n\
             res.send('ok');\n\
             this.eventEmitter.emit('order.created');\n\
             socket.emit('chat');\n\
           }\n\
         }\n",
    );
    let topics: Vec<String> = fi
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::KafkaTopic)
        .map(|n| n.name.to_string())
        .collect();
    assert_eq!(topics, vec!["sum".to_string()], "{topics:?}");
}

#[test]
fn only_clients_the_file_can_identify_are_nest_microservice_clients() {
    let none = index(
        "svc/mail.ts",
        "export class M {\n\
           a() { this.mailClient.send('hello world'); }\n\
           b() { this.httpClient.send('ping'); }\n\
           c() { this.proxy.emit('p'); }\n\
         }\n",
    );
    assert!(events(&none).is_empty(), "{:?}", events(&none));
    let typed = index(
        "svc/typed.ts",
        "import { ClientProxy, ClientKafka } from '@nestjs/microservices';\n\
         import { Inject } from '@nestjs/common';\n\
         export class S {\n\
           constructor(private readonly orders: ClientProxy, @Inject('BILLING') private readonly billing) {}\n\
           a() { this.orders.emit('order.created', {}); }\n\
           b() { return this.billing.send('invoice.get', {}); }\n\
           c() { this.eventBus.emit('internal'); }\n\
         }\n",
    );
    assert_eq!(
        events(&typed),
        vec![ev('P', "invoice.get"), ev('P', "order.created")]
    );
}

#[test]
fn the_kafkajs_shape_needs_a_kafka_shaped_receiver() {
    let fi = index(
        "web/chat.spec.ts",
        "describe('chat', () => {\n\
           it('x', async () => {\n\
             chat.send({ topic: 'greeting' });\n\
             http.post(url).send({ topic: 'help-request' });\n\
             await request(app).post('/help').send({ topic: 'billing' });\n\
             subject.subscribe({ topic: 'x', next: () => {} });\n\
             await producer.send({ topic: '', messages: [] });\n\
           });\n\
         });\n",
    );
    assert!(events(&fi).is_empty(), "{:?}", events(&fi));
    // A Kafka file: only what its Kafka client built is a producer.
    let kafka = index(
        "svc/mixed.ts",
        "import { Kafka } from 'kafkajs';\n\
         const producer = new Kafka({ brokers: [] }).producer();\n\
         export async function f() {\n\
           await producer.send({ topic: 'real', messages: [] });\n\
           await mailer.send({ topic: 'not-kafka' });\n\
         }\n",
    );
    assert_eq!(events(&kafka), vec![ev('P', "real")]);
}

#[test]
fn an_application_bus_in_a_file_that_also_uses_redis_is_not_redis() {
    let fi = index(
        "svc/cqrs.ts",
        "import Redis from 'ioredis';\n\
         export class Svc {\n\
           private readonly cache = new Redis();\n\
           run() {\n\
             this.cache.publish('real:chan', 'x');\n\
             this.eventBus.publish('order.created');\n\
             this.emitter.subscribe('internal');\n\
           }\n\
         }\n",
    );
    assert_eq!(events(&fi), vec![ev('P', "real:chan")]);
    // A type-only import loads nothing at run time.
    let type_only = index(
        "svc/types.ts",
        "import type Redis from 'ioredis';\n\
         export class Svc { run(r: Redis) { this.bus.publish('internal.event'); } }\n",
    );
    assert!(events(&type_only).is_empty());
    // The same module used through typed properties and `duplicate()`.
    let typed = index(
        "svc/typed.ts",
        "import Redis from 'ioredis';\n\
         export class Svc {\n\
           constructor(private readonly redis: Redis) {}\n\
           go() {\n\
             this.redis.publish('a', 'x');\n\
             this.redis.duplicate().subscribe('b');\n\
             this.redis.pipeline().publish('c', 'x').exec();\n\
           }\n\
         }\n",
    );
    assert_eq!(
        events(&typed),
        vec![ev('C', "b"), ev('P', "a"), ev('P', "c")]
    );
    // No client is built in the file (a parameter): the import alone gates.
    let param = index(
        "svc/param.js",
        "const Redis = require('ioredis');\nfunction notify(conn) { conn.publish('p', 'x'); }\n",
    );
    assert_eq!(events(&param), vec![ev('P', "p")]);
}

#[test]
fn constants_respect_scope_and_mutability() {
    let fi = index(
        "svc/scope.ts",
        "const T = 'a';\n\
         let U = 'u';\n\
         U = 'v';\n\
         const V = 'v-const';\n\
         export function f(T: string, producer) { producer.send({ topic: T }); }\n\
         export function g(producer) { producer.send({ topic: U }); }\n\
         export function h(producer) { const V = process.env.V; producer.send({ topic: V }); }\n\
         export function k(producer) { producer.send({ topic: T }); }\n",
    );
    // Only `k` sees the module-level `T`; `U` can change; `V` is shadowed.
    let sends: Vec<(String, String)> = fi
        .producers
        .iter()
        .map(|(i, t)| (fi.nodes[*i].name.to_string(), t.to_string()))
        .collect();
    assert_eq!(sends, vec![("k".to_string(), "a".to_string())], "{sends:?}");
}

#[test]
fn shorthand_local_const_this_field_freeze_and_subscript_names() {
    let fi = index(
        "svc/shapes.ts",
        "const topic = 'top-short';\n\
         const Frozen = Object.freeze({ A: 'frozen-a' });\n\
         const Topics = { 'q-z': 'quoted' } as const;\n\
         export class S {\n\
           private readonly own = 'own-topic';\n\
           async run(producer) {\n\
             const local = 'local-topic';\n\
             await producer.send({ topic, messages: [] });\n\
             await producer.send({ topic: local });\n\
             await producer.send({ topic: this.own });\n\
             await producer.send({ topic: Frozen.A });\n\
             await producer.send({ topic: Topics['q-z'] });\n\
           }\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('P', "frozen-a"),
            ev('P', "local-topic"),
            ev('P', "own-topic"),
            ev('P', "quoted"),
            ev('P', "top-short"),
        ]
    );
}

#[test]
fn an_import_hidden_by_a_parameter_or_a_local_class_is_not_a_queue() {
    let fi = index(
        "svc/shadow.ts",
        "import { Queue } from 'bullmq';\n\
         export function f(Queue) { return new Queue('shadowed-q'); }\n\
         export const real = new Queue('real-q');\n",
    );
    assert_eq!(events(&fi), vec![ev('P', "real-q")]);
    let local = index(
        "web/worker.ts",
        "class Queue {}\nexport const q = new Queue('local');\nexport const w = new Worker('./sw.js');\n",
    );
    assert!(events(&local).is_empty(), "{:?}", events(&local));
}

#[test]
fn nestjs_bull_rabbitmq_and_sqs_decorators() {
    let fi = index(
        "svc/queues.ts",
        "import { Processor, InjectQueue } from '@nestjs/bullmq';\n\
         import { RabbitSubscribe } from '@golevelup/nestjs-rabbitmq';\n\
         import { SqsMessageHandler } from '@ssut/nestjs-sqs';\n\
         @Processor('video')\n\
         export class VideoProcessor {\n\
           @RabbitSubscribe({ exchange: 'x', routingKey: 'rk', queue: 'rabbit-q' })\n\
           onMessage() {}\n\
           @SqsMessageHandler('sqs-q', false)\n\
           async handle() {}\n\
         }\n\
         export class Producer {\n\
           constructor(@InjectQueue('audio') private readonly audio) {}\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('C', "rabbit-q"),
            ev('C', "sqs-q"),
            ev('C', "video"),
            ev('P', "audio"),
        ]
    );
    // Each consumer belongs to what it decorates.
    let owner = |name: &str| {
        fi.consumers
            .iter()
            .find(|(_, t)| t == name)
            .map(|(i, _)| fi.nodes[*i].name.to_string())
    };
    assert_eq!(owner("video").as_deref(), Some("VideoProcessor"));
    assert_eq!(owner("rabbit-q").as_deref(), Some("onMessage"));
    assert_eq!(owner("sqs-q").as_deref(), Some("handle"));
}

#[test]
fn a_bull_queue_is_consumed_where_process_is_called() {
    let fi = index(
        "svc/bull-consumer.js",
        "const Bull = require('bull');\nconst emails = new Bull('emails');\n\
         function start() { emails.process(async (job) => {}); }\n",
    );
    assert_eq!(events(&fi), vec![ev('C', "emails"), ev('P', "emails")]);
}

// ---- review round 2 (PR #82) ------------------------------------------------------------

/// Runs `f` on a thread with the default 2 MB stack (the indexer's pools use no larger one): an unbounded recursion on a
/// hostile file would abort the process (SIGABRT), not fail an assertion.
fn on_small_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("must not overflow the stack")
}

#[test]
fn hostile_chains_neither_overflow_the_stack_nor_record_anything() {
    let chain_call = format!("a{}", ".p()".repeat(3000));
    let chain_member = format!("a{}", ".b".repeat(3000));
    let parens = format!("{}x{}", "(".repeat(900), ")".repeat(900));
    let awaits = "await ".repeat(3000);
    for expr in [chain_call, chain_member, parens, awaits] {
        let code = format!(
            "import Redis from 'ioredis';\nasync function f() {{ const r = new Redis(); const v = {expr}; r.publish({expr}); }}\n"
        );
        let fi = on_small_stack(move || index("svc/hostile.ts", &code));
        // The receiver `r` is a real client, the channel is no name.
        assert!(events(&fi).is_empty(), "{:?}", events(&fi));
    }
}

#[test]
fn a_huge_function_of_new_expressions_and_sends_stays_fast() {
    let mut code = String::from("export function big(producer) {\n  const T = 'topic-a';\n");
    for i in 0..4000 {
        code.push_str(&format!(
            "  const v{i} = compute({i});\n  const o{i} = new Foo(v{i});\n  producer.send({{ topic: T, messages: [] }});\n"
        ));
    }
    code.push_str("}\n");
    let start = std::time::Instant::now();
    let fi = index("svc/big.ts", &code);
    assert_eq!(events(&fi), vec![ev('P', "topic-a")]);
    // Linear: a block is scanned once however many names are looked up in it.
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
}

#[test]
fn a_bare_client_receiver_needs_the_microservices_package() {
    let none = index(
        "svc/gateway.ts",
        "import { Socket } from 'socket.io';\n\
         export class G {\n\
           handle(client: Socket) { client.emit('joined', {}); client.send('hello'); }\n\
         }\n",
    );
    assert!(events(&none).is_empty(), "{:?}", events(&none));
    let axios = index(
        "svc/http.ts",
        "import axios from 'axios';\nexport class H { constructor() { this.client = axios.create(); } go() { this.client.send('hello'); } }\n",
    );
    assert!(events(&axios).is_empty());
    let nest = index(
        "svc/real.ts",
        "import { ClientProxy } from '@nestjs/microservices';\nexport class R { constructor(private client: ClientProxy) {} go() { this.client.emit('user.created', {}); } }\n",
    );
    assert_eq!(events(&nest), vec![ev('P', "user.created")]);
}

#[test]
fn a_client_built_by_a_local_factory_is_not_ruled_out() {
    let fi = index(
        "svc/factory.ts",
        "import Redis from 'ioredis';\n\
         import { Kafka } from 'kafkajs';\n\
         function createRedis() { return new Redis(); }\n\
         function makeProducer() { return new Kafka({ brokers: [] }).producer(); }\n\
         export class S {\n\
           private readonly sub: Redis = new Redis();\n\
           private readonly producer = makeProducer();\n\
           constructor() { this.pub = createRedis(); }\n\
           go() {\n\
             this.pub.publish('via-factory', 'x');\n\
             this.sub.subscribe('typed');\n\
             this.producer.send({ topic: 'kafka-factory', messages: [] });\n\
             this.bus.publish('not-redis');\n\
           }\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('C', "typed"),
            ev('P', "kafka-factory"),
            ev('P', "via-factory")
        ]
    );
}

#[test]
fn a_for_loop_variable_hides_a_module_constant() {
    let fi = index(
        "svc/loop.ts",
        "const T = 'module-level';\n\
         export function f(producer, topics) {\n\
           for (let T = 0; T < 2; T++) { producer.send({ topic: T }); }\n\
         }\n\
         export function g(producer) { producer.send({ topic: T }); }\n",
    );
    assert_eq!(events(&fi), vec![ev('P', "module-level")]);
}

// ---- review round 3 (PR #82) ------------------------------------------------------------

#[test]
fn long_expression_chains_in_a_broker_file_stay_linear() {
    // With a broker mention the detectors run; each link used to climb the whole chain.
    let awaits = format!("const T = {}x;", "await ".repeat(16_000));
    let assigns = format!("let T; T = {}'q';", "T = ".repeat(8_000));
    for body in [awaits, assigns] {
        let code = format!("import {{ Kafka }} from 'kafkajs';\n{body}\n");
        let start = std::time::Instant::now();
        let fi = on_small_stack(move || index("svc/chain.ts", &code));
        assert!(events(&fi).is_empty());
        assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
    }
}

#[test]
fn upper_case_receiver_names_are_still_kafka_shaped() {
    let fi = index(
        "svc/caps.ts",
        "const PRODUCER = make();\nconst CONSUMER = make();\nconst KAFKA = make();\n\
         export async function f() {\n\
           await PRODUCER.send({ topic: 'caps-out' });\n\
           await CONSUMER.subscribe({ topic: 'caps-in' });\n\
           await KAFKA.send({ topic: 'caps-kafka' });\n\
         }\n",
    );
    assert_eq!(
        events(&fi),
        vec![
            ev('C', "caps-in"),
            ev('P', "caps-kafka"),
            ev('P', "caps-out")
        ]
    );
}

#[test]
fn a_local_factory_that_never_touches_the_library_cannot_return_its_client() {
    let fi = index(
        "svc/bus.ts",
        "import Redis from 'ioredis';\n\
         const redis = new Redis();\n\
         function createBus() { return new EventBus(); }\n\
         export class S {\n\
           eventBus = createBus();\n\
           constructor() { this.other = createBus(); }\n\
           f() {\n\
             this.eventBus.publish('app-event', 1);\n\
             this.other.publish('app-event-2', 1);\n\
             redis.publish('real', 'x');\n\
           }\n\
         }\n",
    );
    assert_eq!(events(&fi), vec![ev('P', "real")]);
}

#[test]
fn factory_clients_are_decided_by_whole_identifiers_in_linear_time() {
    // A short library local does not match unrelated words.
    let fi = index(
        "svc/short.ts",
        "import R from 'ioredis';
         const redis = new R();
         function mk() { return new EventBus('NORMAL'); }
         class S { c = mk(); f() { this.c.publish('app', 1); redis.publish('real-short', 1); } }
",
    );
    assert_eq!(events(&fi), vec![ev('P', "real-short")]);
    // ... but a factory that names the library's local does build a client.
    let real = index(
        "svc/real.ts",
        "import R from 'ioredis';
const redis = new R();
function mk() { return new R(); }
         class S { c = mk(); f() { this.c.publish('real', 1); } }
",
    );
    assert_eq!(events(&real), vec![ev('P', "real")]);
    // One large factory called thousands of times, with several libraries imported.
    let mut code = String::from(
        "import Redis from 'ioredis';
import { Kafka } from 'kafkajs';
import mqtt from 'mqtt';
import amqp from 'amqplib';
import { connect } from 'nats';
function mk() {
",
    );
    for i in 0..4000 {
        code.push_str(&format!("  const v{i} = compute({i});\n"));
    }
    code.push_str(
        "  return new EventBus();
}
",
    );
    for i in 0..3000 {
        code.push_str(&format!(
            "const c{i} = mk();
"
        ));
    }
    let start = std::time::Instant::now();
    let fi = index("svc/many.ts", &code);
    assert!(events(&fi).is_empty());
    assert!(start.elapsed().as_secs() < 8, "took {:?}", start.elapsed());
}

// ---- Kinesis, EventBridge, STOMP, Redis lists -----------------------------------------

#[test]
fn kinesis_eventbridge_stomp_and_redis_lists() {
    let kinesis = index(
        "svc/k.ts",
        "import { PutRecordCommand } from '@aws-sdk/client-kinesis';\n\
         export const f = (c: any) => c.send(new PutRecordCommand({ StreamName: 'orders-stream', Data: d }));\n",
    );
    assert_eq!(events(&kinesis), vec![ev('P', "orders-stream")]);

    let bridge = index(
        "svc/b.ts",
        "import { PutEventsCommand } from '@aws-sdk/client-eventbridge';\n\
         export const f = (c: any) => c.send(new PutEventsCommand({ Entries: [{ DetailType: 'order.created' }] }));\n",
    );
    assert_eq!(events(&bridge), vec![ev('P', "order.created")]);

    let stomp = index(
        "svc/s.ts",
        "import { Client } from '@stomp/stompjs';\n\
         const client = new Client({});\n\
         client.publish({ destination: '/queue/x', body: '' });\n\
         client.subscribe('/topic/y', () => {});\n",
    );
    assert_eq!(
        events(&stomp),
        vec![ev('C', "/topic/y"), ev('P', "/queue/x")]
    );

    let redis = index(
        "svc/r.ts",
        "import Redis from 'ioredis';\n\
         const redis = new Redis();\n\
         export async function f() { await redis.lpush('jobs', 'x'); await redis.brpop('done', 0); }\n",
    );
    assert_eq!(events(&redis), vec![ev('C', "done"), ev('P', "jobs")]);

    // No library import, no broker.
    let bare = index(
        "svc/n.ts",
        "export const f = (c: any) => c.lpush('jobs', 'x');\n",
    );
    assert!(events(&bare).is_empty());
}

#[test]
fn redis_blocking_pop_timeouts_and_stomp_generic_names() {
    let redis = index(
        "svc/r2.ts",
        "import Redis from 'ioredis';\nconst redis = new Redis();\nexport const f = () => redis.brpop('q', '5');\n",
    );
    assert_eq!(events(&redis), vec![ev('C', "q")]);
    let ws = index(
        "svc/w.ts",
        "import { Client } from '@stomp/stompjs';\nexport const f = (ws: WebSocket) => ws.send('ping');\n",
    );
    assert!(events(&ws).is_empty());
    let none = index(
        "svc/n2.ts",
        "export const f = (c: any) => c.send(new PutRecordCommand({ StreamName: 'x' }));\n",
    );
    assert!(events(&none).is_empty());
}

#[test]
fn redis_reliable_queue_moves_and_import_gating() {
    let redis = index(
        "svc/mv.ts",
        "import Redis from 'ioredis';\nconst redis = new Redis();\nexport const f = () => redis.rpoplpush('pending', 'working');\nexport const g = () => redis.lmove('a', 'b', 'LEFT', 'RIGHT');\n",
    );
    assert_eq!(
        events(&redis),
        vec![
            ev('C', "a"),
            ev('C', "pending"),
            ev('P', "b"),
            ev('P', "working")
        ]
    );
    for (file, code) in [
        ("svc/e1.ts", "export const f = (c: any) => c.send(new PutEventsCommand({ Entries: [{ DetailType: 'x' }] }));\n"),
        ("svc/e2.ts", "export const f = (c: any) => c.publish({ destination: '/queue/x' });\nexport const g = (c: any) => c.subscribe('/topic/y', () => {});\n"),
        ("svc/e3.ts", "export const f = (c: any) => c.send(new GetShardIteratorCommand({ StreamName: 's' }));\n"),
    ] {
        assert!(events(&index(file, code)).is_empty(), "{file}");
    }
}

#[test]
fn redis_move_spellings_all_link_source_and_destination() {
    for m in [
        "lMove",
        "blmove",
        "bLMove",
        "brpoplpush",
        "bRPopLPush",
        "rPopLPush",
    ] {
        let code = format!(
            "import Redis from 'ioredis';\nconst redis = new Redis();\nexport const f = () => redis.{m}('src', 'dst');\n"
        );
        assert_eq!(
            events(&index("svc/m.ts", &code)),
            vec![ev('C', "src"), ev('P', "dst")],
            "{m}"
        );
    }
}
