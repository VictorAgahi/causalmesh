#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::{
    AuditLogger, CompactStr, ContractEdge, ContractGraph, ContractNode, EdgeConfidence, EdgeKind,
    NodeKind,
};
use mesh_parsers::{AstDecapitator, AstGuard, LanguageKind, MarkdownFormatter, SearchResult};
use std::time::Instant;

const BENCH_ITERATIONS: usize = 500;
const WARMUP_ITERATIONS: usize = 20;

struct BenchmarkResult {
    category: &'static str,
    name: &'static str,
    iterations: usize,
    avg_micros: f64,
    p95_micros: f64,
    min_micros: f64,
    max_micros: f64,
    ops_per_sec: f64,
    throughput_mb_s: Option<f64>,
    token_savings_pct: Option<f64>,
}

fn measure_latencies<F: FnMut()>(mut f: F, iterations: usize) -> (Vec<f64>, f64, f64, f64, f64) {
    // Warmup
    for _ in 0..WARMUP_ITERATIONS {
        f();
    }

    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        let elapsed = start.elapsed().as_nanos() as f64 / 1_000.0; // micros
        times.push(elapsed);
    }

    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total: f64 = times.iter().sum();
    let avg = total / times.len() as f64;
    let min = times[0];
    let max = times[times.len() - 1];
    let p95_idx = ((times.len() as f64) * 0.95) as usize;
    let p95 = times[p95_idx.min(times.len() - 1)];

    (times, avg, p95, min, max)
}

fn main() {
    println!("================================================================================");
    println!("              CausalMesh (MeshMCP) Empirical Benchmark Suite");
    println!(
        "                 Platform: {} | Cores: {}",
        std::env::consts::OS,
        num_cpus::get()
    );
    println!("================================================================================\n");

    let mut results = Vec::new();

    // -------------------------------------------------------------------------
    // 1. Real Polyglot Samples
    // -------------------------------------------------------------------------
    let rust_sample = r#"
    use std::sync::Arc;
    use tokio::sync::RwLock;

    pub trait PaymentGateway: Send + Sync {
        async fn process_transaction(&self, req: PaymentRequest) -> Result<PaymentResponse, PaymentError>;
        fn gateway_name(&self) -> &'static str;
    }

    #[derive(Debug, Clone)]
    pub struct StripeGateway {
        api_key: String,
        timeout_ms: u64,
        client: reqwest::Client,
    }

    impl PaymentGateway for StripeGateway {
        async fn process_transaction(&self, req: PaymentRequest) -> Result<PaymentResponse, PaymentError> {
            if req.amount_cents == 0 {
                return Err(PaymentError::InvalidAmount("Amount must be positive".into()));
            }
            let response = self.client.post("https://api.stripe.com/v1/charges")
                .bearer_auth(&self.api_key)
                .json(&serde_json::json!({
                    "amount": req.amount_cents,
                    "currency": req.currency,
                    "source": req.token,
                }))
                .send()
                .await
                .map_err(|e| PaymentError::Network(e.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                return Err(PaymentError::Declined(format!("Stripe returned HTTP {}", status)));
            }
            Ok(PaymentResponse { id: "ch_test_123".into(), status: "succeeded".into() })
        }

        fn gateway_name(&self) -> &'static str {
            "Stripe-V3-Production"
        }
    }
    "#;

    let ts_sample = r#"
    import { Controller, Post, Body, Headers, UnauthorizedException } from '@nestjs/common';
    import { GrpcMethod } from '@nestjs/microservices';
    import { JwtService } from '@nestjs/jwt';

    @Controller('api/v1/billing')
    export class BillingController {
        constructor(
            private readonly jwtService: JwtService,
            private readonly paymentProcessor: PaymentProcessor,
            private readonly auditService: AuditService,
        ) {}

        @GrpcMethod('BillingRpcService', 'ExecuteInvoicePayment')
        @Post('charge')
        async chargeInvoice(
            @Body() payload: ChargeInvoiceRequest,
            @Headers('authorization') authHeader?: string,
        ): Promise<InvoiceResponse> {
            if (!authHeader || !authHeader.startsWith('Bearer ')) {
                throw new UnauthorizedException('Missing or malformed Authorization header');
            }
            const token = authHeader.substring(7);
            const user = await this.jwtService.verifyAsync(token);
            const result = await this.paymentProcessor.charge({
                userId: user.sub,
                invoiceId: payload.invoiceId,
                amount: payload.amountInCents,
                currency: payload.currency || 'USD',
            });
            await this.auditService.logEvent({
                actor: user.email,
                action: 'BILLING_CHARGE_EXECUTED',
                timestamp: new Date().toISOString(),
                success: result.success,
            });
            return {
                id: result.transactionId,
                status: result.status,
                settledAt: new Date().toISOString(),
            };
        }
    }
    "#;

    let go_sample = r#"
    package service

    import (
        "context"
        "errors"
        "time"
        "google.golang.org/grpc/codes"
        "google.golang.org/grpc/status"
    )

    type OrderServer struct {
        repo OrderRepository
        publisher EventPublisher
        timeout time.Duration
    }

    func (s *OrderServer) CreateOrder(ctx context.Context, req *CreateOrderRequest) (*CreateOrderResponse, error) {
        if req.GetCustomerId() == "" || len(req.GetItems()) == 0 {
            return nil, status.Error(codes.InvalidArgument, "customer_id and items are required")
        }
        ctxTimeout, cancel := context.WithTimeout(ctx, s.timeout)
        defer cancel()

        order := &Order{
            ID: generateUUID(),
            CustomerID: req.GetCustomerId(),
            TotalCents: calculateTotal(req.GetItems()),
            CreatedAt: time.Now().UTC(),
        }
        if err := s.repo.Save(ctxTimeout, order); err != nil {
            return nil, status.Errorf(codes.Internal, "database failed: %v", err)
        }
        event := OrderCreatedEvent{OrderID: order.ID, Total: order.TotalCents}
        if err := s.publisher.Publish(ctxTimeout, "orders.created", event); err != nil {
            return nil, status.Errorf(codes.Internal, "failed to publish event: %v", err)
        }
        return &CreateOrderResponse{OrderId: order.ID, Status: "PENDING"}, nil
    }
    "#;

    let proto_sample = r#"
    syntax = "proto3";

    package mesh.billing.v1;

    option go_package = "github.com/mesh/billing/v1;billingv1";
    option java_multiple_files = true;
    option java_package = "com.mesh.billing.v1";

    service BillingService {
        rpc ChargeInvoice (ChargeInvoiceRequest) returns (ChargeInvoiceResponse);
        rpc RefundPayment (RefundRequest) returns (RefundResponse);
        rpc GetInvoiceHistory (HistoryRequest) returns (stream InvoiceRecord);
    }

    message ChargeInvoiceRequest {
        string invoice_id = 1;
        int64 amount_in_cents = 2;
        string currency = 3;
        string idempotency_key = 4;
    }

    message ChargeInvoiceResponse {
        string transaction_id = 1;
        string status = 2;
        int64 charged_at_epoch_ms = 3;
    }

    message RefundRequest {
        string transaction_id = 1;
        int64 refund_amount_in_cents = 2;
        string reason = 3;
    }

    message RefundResponse {
        string refund_id = 1;
        string status = 2;
    }

    message HistoryRequest {
        string customer_id = 1;
        int32 page_size = 2;
    }

    message InvoiceRecord {
        string invoice_id = 1;
        int64 amount = 2;
        string status = 3;
    }
    "#;

    // -------------------------------------------------------------------------
    // Benchmark Group 1: AST Decapitation & Token Savings
    // -------------------------------------------------------------------------
    println!(">>> Running AST Decapitation & Token Economy Benchmarks...");

    let lang_cases = [
        ("TypeScript", ts_sample, LanguageKind::TypeScript),
        ("Rust", rust_sample, LanguageKind::Rust),
        ("Go", go_sample, LanguageKind::Go),
    ];

    for (lang_name, source, lang_kind) in lang_cases {
        let raw_chars = source.len();
        let raw_tokens = raw_chars / 4;
        let mut decap_out = String::new();

        let (_, avg, p95, min, max) = measure_latencies(
            || {
                decap_out = AstDecapitator::decapitate_auto(source, lang_kind, false);
            },
            BENCH_ITERATIONS,
        );

        let decap_chars = decap_out.len();
        let decap_tokens = decap_chars / 4;
        let tokens_saved = raw_tokens.saturating_sub(decap_tokens);
        let savings_pct = (tokens_saved as f64 / raw_tokens as f64) * 100.0;
        let mb_s = (raw_chars as f64 / (1024.0 * 1024.0)) / (avg / 1_000_000.0);

        results.push(BenchmarkResult {
            category: "AST Decapitation",
            name: lang_name,
            iterations: BENCH_ITERATIONS,
            avg_micros: avg,
            p95_micros: p95,
            min_micros: min,
            max_micros: max,
            ops_per_sec: 1_000_000.0 / avg,
            throughput_mb_s: Some(mb_s),
            token_savings_pct: Some(savings_pct),
        });
    }

    // -------------------------------------------------------------------------
    // Benchmark Group 2: Lexical Guard Pre-Checks
    // -------------------------------------------------------------------------
    println!(">>> Running AstGuard Lexical Pre-Check Benchmarks...");
    let proto_bytes = proto_sample.as_bytes();
    let (_, avg_guard, p95_guard, min_guard, max_guard) = measure_latencies(
        || {
            let _depth = AstGuard::max_nesting_depth(proto_bytes);
        },
        BENCH_ITERATIONS * 2,
    );

    let proto_mb_s = (proto_bytes.len() as f64 / (1024.0 * 1024.0)) / (avg_guard / 1_000_000.0);
    results.push(BenchmarkResult {
        category: "Lexical Guard",
        name: "AstGuard::max_nesting_depth",
        iterations: BENCH_ITERATIONS * 2,
        avg_micros: avg_guard,
        p95_micros: p95_guard,
        min_micros: min_guard,
        max_micros: max_guard,
        ops_per_sec: 1_000_000.0 / avg_guard,
        throughput_mb_s: Some(proto_mb_s),
        token_savings_pct: None,
    });

    // -------------------------------------------------------------------------
    // Benchmark Group 3: Contract Graph Topology Queries (1,000 Nodes, 500 Repos)
    // -------------------------------------------------------------------------
    println!(">>> Running In-Memory Contract Graph Benchmarks (1,000 Nodes)...");
    let mut graph = ContractGraph::new();

    for i in 0..1000u32 {
        let repo_id = (i % 500) as u16;
        let node_id = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new(format!("MicroServiceNode{i}")),
            kind: if i % 4 == 0 {
                NodeKind::GrpcMethod
            } else if i % 4 == 1 {
                NodeKind::KafkaTopic
            } else {
                NodeKind::ServiceClass
            },
            file_path: std::path::PathBuf::from(format!("services/srv_{repo_id}/node_{i}.rs"))
                .into(),
            line_start: 1,
            line_end: 100,
            package: CompactStr::new(format!("corp.mesh.srv_{repo_id}")),
            repo_id,
            signature: Some("pub fn handle()".into()),
            docstring: None,
        });

        if i > 0 && i % 2 == 0 {
            graph.add_dependency(node_id, "EnterpriseContractModel");
        }
        if i > 0 && i % 5 == 0 {
            graph.add_edge(ContractEdge {
                from: node_id,
                to: node_id - 1,
                kind: EdgeKind::Implements,
                metadata: None,
                confidence: EdgeConfidence::Exact,
            });
        }
    }

    let (_, avg_dep, p95_dep, min_dep, max_dep) = measure_latencies(
        || {
            let deps = graph.find_dependents("EnterpriseContractModel");
            std::hint::black_box(deps);
        },
        BENCH_ITERATIONS,
    );

    results.push(BenchmarkResult {
        category: "Contract Graph",
        name: "find_dependents (O(1) resolution)",
        iterations: BENCH_ITERATIONS,
        avg_micros: avg_dep,
        p95_micros: p95_dep,
        min_micros: min_dep,
        max_micros: max_dep,
        ops_per_sec: 1_000_000.0 / avg_dep,
        throughput_mb_s: None,
        token_savings_pct: None,
    });

    let (_, avg_grpc, p95_grpc, min_grpc, max_grpc) = measure_latencies(
        || {
            let trace = graph.analyze_grpc("MicroServiceNode4");
            std::hint::black_box(trace);
        },
        BENCH_ITERATIONS,
    );

    results.push(BenchmarkResult {
        category: "Contract Graph",
        name: "analyze_grpc (Pipeline trace)",
        iterations: BENCH_ITERATIONS,
        avg_micros: avg_grpc,
        p95_micros: p95_grpc,
        min_micros: min_grpc,
        max_micros: max_grpc,
        ops_per_sec: 1_000_000.0 / avg_grpc,
        throughput_mb_s: None,
        token_savings_pct: None,
    });

    // -------------------------------------------------------------------------
    // Benchmark Group 4: Cryptographic Audit Hash Chain Appends
    // -------------------------------------------------------------------------
    println!(">>> Running Cryptographic Audit Logger Benchmarks...");
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let audit_file = temp_dir.path().join("bench_audit.log");
    let logger = AuditLogger::new(Some(audit_file.clone())).expect("init audit logger");

    let (_, avg_audit, p95_audit, min_audit, max_audit) = measure_latencies(
        || {
            let _ = logger.record_entry(
                "bench-session",
                Some("trace-001"),
                "smart_search",
                "{\"query\":\"bench_symbol\"}",
                "SUCCESS",
                vec!["crates/mesh-core/src/lib.rs".to_string()],
                0,
            );
        },
        BENCH_ITERATIONS,
    );

    let is_valid = AuditLogger::verify_log_file(&audit_file).expect("verify chain");
    assert!(is_valid, "Audit chain must be valid!");

    results.push(BenchmarkResult {
        category: "Audit Logging",
        name: "record_entry (Flock + SHA-256)",
        iterations: BENCH_ITERATIONS,
        avg_micros: avg_audit,
        p95_micros: p95_audit,
        min_micros: min_audit,
        max_micros: max_audit,
        ops_per_sec: 1_000_000.0 / avg_audit,
        throughput_mb_s: None,
        token_savings_pct: None,
    });

    // -------------------------------------------------------------------------
    // Benchmark Group 5: Markdown 48 KB Affordance Truncation
    // -------------------------------------------------------------------------
    println!(">>> Running Markdown Output Formatting & Truncation...");
    let search_results: Vec<SearchResult> = (0..100)
        .map(|i| SearchResult {
            file_path: format!("services/srv_{i}/handler_{i}.ts"),
            line_start: i * 10,
            line_end: i * 10 + 30,
            language: "typescript".to_string(),
            snippet: format!("export class Handler{i} {{ execute(): void {{ /* logic */ }} }}"),
        })
        .collect();

    let mut formatted_len = 0;
    let (_, avg_fmt, p95_fmt, min_fmt, max_fmt) = measure_latencies(
        || {
            let out =
                MarkdownFormatter::format_search_results("Handler", "services", &search_results);
            formatted_len = out.len();
        },
        BENCH_ITERATIONS,
    );

    assert!(formatted_len <= 48 * 1024, "Output must be <= 48 KB");

    results.push(BenchmarkResult {
        category: "Markdown Formatting",
        name: "format_search_results (48 KB bound)",
        iterations: BENCH_ITERATIONS,
        avg_micros: avg_fmt,
        p95_micros: p95_fmt,
        min_micros: min_fmt,
        max_micros: max_fmt,
        ops_per_sec: 1_000_000.0 / avg_fmt,
        throughput_mb_s: None,
        token_savings_pct: None,
    });

    // -------------------------------------------------------------------------
    // Output Structured Report
    // -------------------------------------------------------------------------
    println!("\n=========================================================================================================");
    println!("                                     EMPIRICAL BENCHMARK RESULTS");
    println!("=========================================================================================================");
    println!(
        "{:<18} | {:<28} | {:>7} | {:>7} | {:>7} | {:>7} | {:>9} | {:>10} | {:>8}",
        "Category",
        "Benchmark",
        "Avg(µs)",
        "Min(µs)",
        "Max(µs)",
        "p95(µs)",
        "Ops/sec",
        "Throughput",
        "Tokens"
    );
    println!("---------------------------------------------------------------------------------------------------------");

    for r in &results {
        let tp_str = match r.throughput_mb_s {
            Some(mb) => format!("{:.1} MB/s", mb),
            None => "-".to_string(),
        };
        let tok_str = match r.token_savings_pct {
            Some(pct) => format!("-{:.1}%", pct),
            None => "-".to_string(),
        };
        let name_with_iters = format!("{} (n={})", r.name, r.iterations);
        println!(
            "{:<18} | {:<28} | {:>7.2} | {:>7.2} | {:>7.2} | {:>7.2} | {:>9.0} | {:>10} | {:>8}",
            r.category,
            name_with_iters,
            r.avg_micros,
            r.min_micros,
            r.max_micros,
            r.p95_micros,
            r.ops_per_sec,
            tp_str,
            tok_str
        );
    }
    println!("=========================================================================================================\n");
    println!("✔ All benchmark runs completed deterministically with zero artificial mocks.");
}
