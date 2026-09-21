package com.shop.billing;

/**
 * Distributed Billing Saga Coordinator (Java / Spring Boot)
 * Orchestrates payment confirmation, VAT tax calculations, and invoice emissions.
 */
public class BillingSaga {

    // Kafka template for publishing downstream events
    private final KafkaTemplate kafkaTemplate = new KafkaTemplate();

    public void processSaga() {
        System.out.println("Initializing OrderBillingSaga");
    }

    @KafkaListener(topics = "order-created-topic")
    public void handleOrderCreated(String orderId, double amount) {
        System.out.println("[Saga] Received order-created-topic event for order: " + orderId);
        
        // Calculate VAT tax and generate PDF invoice
        String invoice = "INV-" + orderId;

        // Emit invoice generated event to downstream financial ledger
        kafkaTemplate.send("invoice-generated-topic", invoice);
    }

    static class KafkaTemplate {
        public void send(String topic, Object payload) {
            System.out.println("Emitted event to topic: " + topic);
        }
    }
}
