package com.acme.billing;

import com.acme.shared.Money;

public class OrderListener {
    @KafkaListener(topics = "orders.created")
    public void onOrderCreated(String payload) {
        Money total = Money.zero();
    }
}
