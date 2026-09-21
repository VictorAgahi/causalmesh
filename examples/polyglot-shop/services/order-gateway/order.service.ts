import { Injectable } from '@nestjs/common';
import { GrpcMethod } from '@nestjs/microservices';
import { Producer } from 'kafkajs';

export interface CreateOrderDto {
  order_id: string;
  customer_id: string;
  amount: number;
}

@Injectable()
export class OrderService {
  constructor(private readonly kafkaProducer: Producer) {}

  @GrpcMethod('CheckoutService', 'CreateOrder')
  async createOrder(data: CreateOrderDto) {
    // Ingest order and emit causal event to Kafka stream
    await this.kafkaProducer.send({
      topic: 'order-created-topic',
      messages: [{ key: data.order_id, value: JSON.stringify(data) }],
    });

    return { order_id: data.order_id, status: 'PENDING_PAYMENT' };
  }
}
