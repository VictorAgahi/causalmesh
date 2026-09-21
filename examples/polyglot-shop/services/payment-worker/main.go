package main

import (
	"context"
	"fmt"
)

// PaymentWorker consumes events from order-created-topic and triggers payment settlement
type PaymentWorker struct {
	topic string
}

func NewPaymentWorker() *PaymentWorker {
	return &PaymentWorker{topic: "order-created-topic"}
}

func (w *PaymentWorker) HandleOrderCreated(ctx context.Context, orderId string, amount float64) error {
	fmt.Printf("Processing payment of $%.2f for order %s\n", amount, orderId)
	return nil
}

func main() {
	worker := NewPaymentWorker()
	_ = worker.HandleOrderCreated(context.Background(), "ord-1234", 99.99)
}
