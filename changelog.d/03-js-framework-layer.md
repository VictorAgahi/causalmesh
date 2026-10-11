### JS/TS parser: framework layer and message brokers (7.1.0 stack, PR 3/7)
- **A rule-table detector layer** (`languages/js_frameworks`) replaces the hard-coded kafkajs / BullMQ /
  NestJS branches of `visit_node`. Each rule names the modules the file must load, the method or
  constructor, the side of the topic (`Produce` / `Consume`), and how to read the name from the arguments.
  A generic `.publish()` / `.subscribe()` / `.send()` is a broker call only in a file that loads that
  broker's library (`require`, `import`, `import()` and aliases are all followed).
- **Brokers covered**: Kafka family (kafkajs incl. `sendBatch` and `topics: []`, node-rdkafka,
  `@confluentinc/kafka-javascript`, kafka-node, `@platformatic/kafka`; Redpanda through these, it speaks the Kafka
  protocol), Redis (`ioredis`, `redis`, `@redis/client`, `@upstash/redis`: pub/sub, `psubscribe`, streams
  `xadd` / `xread` / `xreadgroup` in both client styles), NATS (`nats`, `@nats-io/*`, core and JetStream),
  MQTT, RabbitMQ (`amqplib`), BullMQ / Bull / bee-queue, AWS SQS / SNS (SDK v2 and v3, literal URLs / ARNs),
  Google Pub/Sub, Apache Pulsar, Azure Service Bus and Event Hubs, NestJS `ClientProxy.emit/send`.
- **Names are resolved within the file**: string literals, template literals, `+` concatenation,
  `const X = '…'`, `{ A: '…' } as const` objects, string enums and `static readonly` class members.
  A reference to an *imported* constant (`Topics.ORDER_PLACED`, `ORDER_PLACED`) is kept as written for
  the graph to reduce, as for Java; a parameter, `process.env.X`, `config.topic` or an undeclared name is
  still not a name and records nothing.
- **Fixes**: `res.send('ok')` was recorded as a NestJS topic (it now needs a client-shaped receiver or a
  `@nestjs/microservices` import); `new Queue(...)` imported from another library (`typescript-collections`)
  is no longer a BullMQ queue; event nodes (`EventStream`) are no longer chosen as the owner of a call site.
