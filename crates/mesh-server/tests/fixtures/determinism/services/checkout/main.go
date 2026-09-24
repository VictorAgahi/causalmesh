package main

import (
	"context"

	pb "example.com/checkout/genproto/shop/v1"
	usersv1 "example.com/checkout/genproto/users/v1"
)

type checkoutServer struct{}

func (s *checkoutServer) PlaceOrder(ctx context.Context, req *pb.PlaceOrderRequest) (*pb.PlaceOrderResponse, error) {
	return nil, nil
}

func (s *checkoutServer) GetStatus(ctx context.Context, req *pb.StatusRequest) (*pb.StatusResponse, error) {
	return nil, nil
}

func main() {
	pb.RegisterCheckoutServiceServer(grpcServer, &checkoutServer{})
	admin := usersv1.NewAdminServiceClient(conn)
	_ = admin
}

func publishOrder(ctx context.Context, w *kafka.Writer) {
	w.WriteMessages(ctx, kafka.Message{Topic: "orders.created"})
}
