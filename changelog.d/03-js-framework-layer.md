### JS/TS parser: framework layer and message brokers (7.1.0 stack, PR 3/7)
- **A rule-table detector layer** (`languages/js_frameworks`) replaces the hard-coded kafkajs / BullMQ /
  NestJS branches of `visit_node`. Each rule names the modules the file must load, the method,
  constructor or decorator, the side of the topic (`Produce` / `Consume`), and how to read the name from
  the arguments. The per-file context is built lazily, on the first call site a rule could match.
- **Gated on the file and on the receiver**: a generic `.publish()` / `.subscribe()` / `.send()` is a
  broker call only in a file that loads that broker's library (`require`, `import`, `import()`, aliases and
  destructuring are followed; `import type` loads nothing) *and*, when the file builds a client of that
  library (`new Redis()`, `createClient()`, `connect()`, `kafka.producer()`, a typed property or
  parameter, `.duplicate()` / `.pipeline()` chains), only on those clients. An application event bus in a
  file that also uses Redis for caching is no longer Redis. When the file builds no such client (an
  untyped parameter) the import alone gates, as before.
- **Brokers covered**: Kafka family (kafkajs incl. `sendBatch` and `topics: []`, node-rdkafka,
  `@confluentinc/kafka-javascript`, kafka-node, `@platformatic/kafka`; Redpanda through these, it speaks
  the Kafka protocol), Redis (`ioredis`, `redis`, `@redis/client`, `@upstash/redis` and the Nest wrappers:
  pub/sub, `psubscribe`, streams `xadd` / `xread` / `xreadgroup` in both client styles), NATS (`nats`,
  `@nats-io/*`, core and JetStream), MQTT, RabbitMQ (`amqplib`), BullMQ / Bull / bee-queue (incl. Bull
  `queue.process()` as the consumer of the queue the receiver was built for), AWS SQS / SNS (SDK v2 and v3,
  literal URLs / ARNs), Google Pub/Sub, Apache Pulsar, Azure Service Bus and Event Hubs, NestJS
  microservice clients, and the queue decorators `@Processor` / `@InjectQueue` (`@nestjs/bull`,
  `@nestjs/bullmq`), `@RabbitSubscribe` / `@RabbitRPC` (`@golevelup/nestjs-rabbitmq`) and
  `@SqsMessageHandler` (`@ssut/nestjs-sqs`), each attributed to the class or method it decorates.
- **Names are resolved within the file, scope-aware**: string literals, template literals, `+`
  concatenation, module-level `const X = '…'`, `{ A: '…' } as const` / `Object.freeze` objects, string
  enums, `static readonly` and `this.field` class members, function-local `const`s, `Topics['q-z']` and the
  shorthand `{ topic }`. `let` / `var`, a parameter or a local declaration of the same name hide the
  module-level constant. A reference to an *imported* constant (`Topics.ORDER_PLACED`, `ORDER_PLACED`) is
  kept as written for the graph to reduce, as for Java; a parameter, `process.env.X`, `config.topic` or an
  undeclared name is not a name and records nothing, nor does an empty one.
- **Fixes**: `res.send('ok')`, `this.eventEmitter.emit(...)` and `socket.emit(...)` were recorded as NestJS
  topics in any file that merely imported `@nestjs/microservices` (a microservice client must now be
  typed `ClientProxy` / `ClientKafka` / …, injected with `@Inject(...)`, or named like a client in such a
  file); `chat.send({ topic })` and a supertest `.post(...).send({ topic })` were Kafka producers (the
  kafkajs shape now needs a Kafka import with a client the file built, or a receiver named
  `producer` / `consumer` / `kafka`); a `Queue` imported from another library, a local `class Queue`, or
  `new Worker('./sw.js')` is no longer a BullMQ queue; event nodes (`EventStream`) are no longer chosen as
  the owner of a call site.
- **Known limits**: a Kafka topic, a Redis channel and a queue sharing a name still meet on one hub in the
  graph (the hub key is the lowercased name, whatever the broker): a follow-up. Wildcard subjects / topics
  (`orders.*`, `sensors/+/temp`) are recorded literally. A NestJS client reached as `this.orderService.emit`
  (a wrapper service) is no longer recorded.
