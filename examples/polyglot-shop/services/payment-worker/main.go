package main

import (
	"context"
	"fmt"

	pb "github.com/polyglot-shop/gen/go/shop/v1"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

// PaymentServer implements the protobuf PaymentService contract
type PaymentServer struct{}

func (s *PaymentServer) ProcessPayment(ctx context.Context, orderID string, amount float64) (string, error) {
	fmt.Printf("[PaymentService.ProcessPayment] Processing $%.2f for order %s\n", amount, orderID)
	return fmt.Sprintf("pay-%s", orderID), nil
}

// PaymentWorker handles event stream consumption and settlement dispatch
type PaymentWorker struct {
	consumeTopic string
	produceTopic string
}

func NewPaymentWorker() *PaymentWorker {
	return &PaymentWorker{
		consumeTopic: "order-created-topic",
		produceTopic: "payment-settled-topic",
	}
}

func (w *PaymentWorker) HandleOrderCreated(ctx context.Context, orderID string, amount float64) error {
	server := &PaymentServer{}
	paymentID, err := server.ProcessPayment(ctx, orderID, amount)
	if err != nil {
		return err
	}

	fmt.Printf("[Kafka -> %s] Emitting PaymentSettledEvent for %s (payment: %s)\n", w.produceTopic, orderID, paymentID)
	return nil
}

func main() {
	// Serves the PaymentService gRPC contract (proto/payment.proto).
	grpcServer := grpc.NewServer()
	pb.RegisterPaymentServiceServer(grpcServer, &PaymentServer{})

	// Synchronous client of the inventory-manager's InventoryService.
	conn, err := grpc.NewClient("inventory-manager:50051", grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		return
	}
	defer conn.Close()
	_ = pb.NewInventoryServiceClient(conn)

	worker := NewPaymentWorker()
	_ = worker.HandleOrderCreated(context.Background(), "ord-9921", 149.50)
}
