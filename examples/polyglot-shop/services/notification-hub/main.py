"""
Notification Hub Microservice (Python / FastAPI)
Consumes events across the checkout pipeline to send multi-channel notifications (Email, SMS, Push).
"""

from typing import Dict, Any


def kafka_consumer(topic: str):
    """Decorator marking consumer of a Kafka topic"""
    def decorator(fn):
        fn.__topic__ = topic
        return fn
    return decorator


class NotificationDispatcher:
    @kafka_consumer("order-created-topic")
    def on_order_created(self, event: Dict[str, Any]):
        print(f"[Email] Order confirmation sent for order: {event.get('order_id')}")

    @kafka_consumer("payment-settled-topic")
    def on_payment_settled(self, event: Dict[str, Any]):
        print(f"[SMS] Payment receipt delivered for order: {event.get('order_id')}")

    @kafka_consumer("stock-reserved-topic")
    def on_stock_reserved(self, event: Dict[str, Any]):
        print(f"[Push] Shipping tracking initiated for order: {event.get('order_id')}")


def main():
    dispatcher = NotificationDispatcher()
    print("Notification hub actively polling Kafka event topics...")


if __name__ == "__main__":
    main()
