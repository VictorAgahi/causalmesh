# Polyglot Shop — MeshMCP example workspace

A small example workspace: three `.proto` contracts and five services in five languages (TypeScript / NestJS, Go, Rust, Python, Java / Spring Boot), wired together by gRPC calls and Kafka topics. The services are minimal stubs written to exercise MeshMCP's extraction, not runnable applications. It is also a test fixture: `crates/mesh-server/tests/impact_matrix.rs` and `scripts/determinism.sh` run against it.

```
                      ┌────────────────────────────────────────┐
                      │ shop.checkout.v1 / shop.payment.v1     │
                      │        (Protobuf Contracts)            │
                      └──────────────────┬─────────────────────┘
                                         │ Implements / CallsRpc
                                         ▼
┌──────────────────────┐   gRPC Call    ┌──────────────────────┐
│    order-gateway     │───────────────>│    payment-worker    │
│ (TypeScript / NestJS)│                │         (Go)         │
└──────────┬───────────┘                └──────────┬───────────┘
           │ Produces                              │ Produces
           ▼                                       ▼
┌──────────────────────┐                ┌──────────────────────┐
│ order-created-topic  │                │payment-settled-topic │
└──────────┬───────────┘                └──────────┬───────────┘
           │                                       │
     ┌─────┴──────────────────┐                    │
     │ Consumes               │ Consumes           │ Consumes
     ▼                        ▼                    ▼
┌──────────────────┐   ┌──────────────┐   ┌────────────────────┐
│   billing-saga   │   │ notification │<──│ inventory-manager  │
│(Java/Spring Boot)│   │  hub (Python)│   │       (Rust)       │
└────────┬─────────┘   └──────────────┘   └─────────┬──────────┘
         │ Produces           ▲                     │ Produces
         ▼                    │ Consumes            ▼
┌────────────────────────┐    │           ┌────────────────────┐
│invoice-generated-topic │    └───────────│stock-reserved-topic│
└────────────────────────┘                └────────────────────┘
```

## Architecture & Services
- **`proto/`**:
  - `checkout.proto`: `CheckoutService` definition (`CreateOrder`, `GetOrderStatus`).
  - `payment.proto`: `PaymentService` definition (`ProcessPayment`, `RefundPayment`).
  - `inventory.proto`: `InventoryService` definition (`ReserveStock`, `ReleaseStock`).
- **`services/order-gateway`** (*TypeScript / NestJS*):
  - Implements `CheckoutService.CreateOrder` (`@GrpcMethod`).
  - Calls `PaymentService.ProcessPayment` via gRPC client stub.
  - Emits events to Kafka topic `order-created-topic`.
- **`services/payment-worker`** (*Go*):
  - Registers `PaymentService` (`pb.RegisterPaymentServiceServer`), implementing `ProcessPayment`.
  - Opens an `InventoryService` client (`pb.NewInventoryServiceClient`).
  - Consumes `order-created-topic`.
  - Emits settlement events to `payment-settled-topic`.
- **`services/inventory-manager`** (*Rust*):
  - Implements the `InventoryService` trait (tonic style), including `ReserveStock`.
  - Consumes `payment-settled-topic`.
  - Emits stock reservation events to `stock-reserved-topic`.
- **`services/notification-hub`** (*Python*):
  - Consumes `order-created-topic` (order confirmations).
  - Consumes `payment-settled-topic` (payment receipts).
  - Consumes `stock-reserved-topic` (dispatch notifications).
- **`services/billing-saga`** (*Java / Spring Boot*):
  - `@KafkaListener` orchestrator for `order-created-topic`.
  - Coordinates tax calculations and publishes to `invoice-generated-topic`.

---

## Try it

### 1. View Interactive Graph Visualization
From the repository root:
```bash
mesh-mcp graph --config examples/polyglot-shop/mesh-mcp.toml --open
```
*Writes a standalone HTML topology and opens it in the default browser.*

### 2. Export Mermaid Markdown
```bash
mesh-mcp graph --config examples/polyglot-shop/mesh-mcp.toml --format mermaid
```

### 3. Check Monorepo Health
```bash
mesh-mcp doctor --config examples/polyglot-shop/mesh-mcp.toml
```

### 4. Query it as an agent would

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"analyze_impact","arguments":{"target":"ProcessPayment"}}}' \
  | mesh-mcp run --standalone --config examples/polyglot-shop/mesh-mcp.toml
```

The Kafka producers and consumers in this example are declared through
`[[engines.contracts.patterns]]` regexes in `mesh-mcp.toml` (one set per language), to show how
in-house conventions are described. The gRPC wiring is detected natively.
