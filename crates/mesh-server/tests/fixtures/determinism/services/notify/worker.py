from confluent_kafka import Consumer
from shared.money import Money


def run():
    consumer = Consumer({})
    consumer.subscribe(["orders.created"])
    return Money
