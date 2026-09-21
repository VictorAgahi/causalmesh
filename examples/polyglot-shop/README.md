# Polyglot Shop &mdash; CausalMesh Demonstration Monorepo

This sample monorepo demonstrates cross-service contract reconciliation and causal impact tracing across multiple programming languages (**Protobuf**, **TypeScript / NestJS**, **Go**, and **Rust**).

## Architecture Highlights
- **`proto/checkout.proto`**: gRPC definition of `CheckoutService` and event schema `OrderCreatedEvent`.
- **`services/order-gateway`** (*TypeScript / NestJS*): Exposes `@GrpcMethod('CheckoutService', 'CreateOrder')` and emits to Kafka topic `order-created-topic`.
- **`services/payment-worker`** (*Go*): Consumes `order-created-topic` and executes transaction reconciliation.
- **`services/inventory-manager`** (*Rust*): Subscribes to stock decrement flows.

---

## 🚀 Instant Verification (10-Second Test)

### 1. View Interactive Graph Visualization
From the repository root or inside this folder:
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
