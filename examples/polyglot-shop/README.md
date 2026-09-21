# Polyglot Shop &mdash; CausalMesh Demonstration Monorepo

This sample monorepo demonstrates real-world cross-service contract reconciliation, gRPC pipeline tracing, Kafka event streaming, and distributed saga coordination across **5 programming languages** (**Protobuf**, **TypeScript / NestJS**, **Go**, **Rust**, **Python / FastAPI**, and **Java / Spring Boot**).

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
  - Implements `PaymentService.ProcessPayment`.
  - Consumes `order-created-topic`.
  - Emits settlement events to `payment-settled-topic`.
- **`services/inventory-manager`** (*Rust*):
  - Implements `InventoryService.ReserveStock`.
  - Consumes `payment-settled-topic`.
  - Emits stock reservation events to `stock-reserved-topic`.
- **`services/notification-hub`** (*Python / FastAPI*):
  - Consumes `order-created-topic` (order confirmations).
  - Consumes `payment-settled-topic` (payment receipts).
  - Consumes `stock-reserved-topic` (dispatch notifications).
- **`services/billing-saga`** (*Java / Spring Boot*):
  - `@KafkaListener` orchestrator for `order-created-topic`.
  - Coordinates tax calculations and publishes to `invoice-generated-topic`.

---

## 🚀 Instant Verification (10-Second Test)

### 1. View Interactive Graph Visualization
From the repository root:
```bash
mesh-mcp graph --config examples/polyglot-shop/mesh-mcp.toml --open
```
*Opens an interactive, dark-mode SVG/Canvas topology map in your default browser.*

### 2. Export Mermaid Markdown
```bash
mesh-mcp graph --config examples/polyglot-shop/mesh-mcp.toml --format mermaid
```

### 3. Check Monorepo Health
```bash
mesh-mcp doctor --config examples/polyglot-shop/mesh-mcp.toml
```
