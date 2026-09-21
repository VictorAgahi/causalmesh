import { Injectable } from '@nestjs/common';
import { GrpcMethod, ClientGrpc } from '@nestjs/microservices';
import { Producer } from 'kafkajs';

export interface CreateOrderDto {
  order_id: string;
  customer_id: string;
  amount: number;
}

export interface PaymentServiceClient {
  ProcessPayment(data: { order_id: string; amount: number; currency: string }): Promise<{ payment_id: string; status: string }>;
}

@Injectable()
export class OrderService {
  private paymentClient: PaymentServiceClient;

  constructor(
    private readonly kafkaProducer: Producer,
    private readonly grpcClient: ClientGrpc
  ) {
    this.paymentClient = this.grpcClient.getService<PaymentServiceClient>('PaymentService');
  }

  @GrpcMethod('CheckoutService', 'CreateOrder')
  async createOrder(data: CreateOrderDto) {
    // 1. Synchronous causal gRPC call to payment-worker
    const payment = await this.paymentClient.ProcessPayment({
      order_id: data.order_id,
      amount: data.amount,
      currency: 'USD',
    });

    // 2. Asynchronous causal event dispatch to Kafka stream
    await this.kafkaProducer.send({
      topic: 'order-created-topic',
      messages: [{ key: data.order_id, value: JSON.stringify({ ...data, payment_status: payment.status }) }],
    });

    return { order_id: data.order_id, status: 'ORDER_PLACED' };
  }

  @GrpcMethod('CheckoutService', 'GetOrderStatus')
  async getOrderStatus(data: { order_id: string }) {
    return { order_id: data.order_id, status: 'COMPLETED' };
  }
}
